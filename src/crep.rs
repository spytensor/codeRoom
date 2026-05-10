//! CodeRoom Event Protocol — the normalized event stream emitted by every
//! engine adapter and consumed by the message bus, REPL, and patch logic.
//!
//! See `docs/architecture.md` § "CodeRoom Event Protocol (CREP)".
//!
//! Wire format is JSON: each event serializes to a single object with a
//! `"type"` discriminator and snake_case field names. The append-only
//! `.coderoom/messages.jsonl` log stores events in this exact shape.

use serde::{Deserialize, Serialize};

/// A single event in the CodeRoom Event Protocol stream.
///
/// Variants intentionally use a single `#[serde(tag = "type")]` discriminator
/// so the JSONL log is grep-friendly:
///
/// ```text
/// jq -r 'select(.type=="role_spoke") | .text' messages.jsonl
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CrepEvent {
    /// A role's subprocess is up and the system prompt has been loaded.
    /// First event for any role; emitted exactly once per session.
    RoleStarted {
        /// Configured name of the role (e.g. `"backend"`, `"security"`).
        role: String,
        /// Engine driving this role: `"cc"`, `"codex"`, or `"gemini"`.
        engine: String,
        /// Model identifier as reported by the engine.
        model: String,
        /// Engine-issued session id; stable across turns of the same session.
        session_id: String,
        /// Hash of the composed system prompt (priors + patches + journal).
        /// Used to detect drift between intended and actual role identity.
        priors_hash: String,
    },
    /// Role supplied a display title for the current work unit.
    ///
    /// This is metadata for terminal cards, not user-visible chat. The
    /// title is derived from a `cr-task` block or adapter-native work
    /// signal and must not be treated as protocol identity.
    WorkTitle {
        /// Configured name of the role whose current work title changed.
        role: String,
        /// Sanitized one-line work title.
        title: String,
    },
    /// The role emitted a final assistant turn (the LLM finished its
    /// response for the current user message).
    ///
    /// `mentions` is the list of `@<name>` references parsed out of the
    /// reply text; the wrapper uses this to route briefs to other roles.
    RoleSpoke {
        /// Configured name of the role that spoke.
        role: String,
        /// Final assistant-turn text. UI and message-bus consumers render
        /// this directly.
        text: String,
        /// Parsed `@<name>` references from `text`, in order of first
        /// appearance, deduplicated.
        mentions: Vec<String>,
        /// Cost of this turn in USD (engine-reported).
        cost_usd: f64,
        /// Tokens served from prompt cache for this turn (engine-reported).
        cache_read: u64,
    },
    /// Engine asked to call a tool; the wrapper's PreToolUse hook saw it
    /// before the tool ran. May be followed by either `ToolCallExecuted`
    /// (if approved) or `PermissionDenied` (if vetoed).
    ToolCallProposed {
        /// Configured name of the role proposing the call.
        role: String,
        /// Engine-native tool identifier (e.g. `"Bash"`, `"Edit"`, `"Read"`).
        tool_name: String,
        /// Engine-native tool input as opaque JSON; preserved verbatim so
        /// the PreToolUse gate can pattern-match on it.
        tool_input: serde_json::Value,
        /// Engine-issued tool-use id; pairs this proposal with its later
        /// `ToolCallExecuted` or `PermissionDenied` event.
        tool_use_id: String,
    },
    /// Tool call ran to completion. `output_summary` is a one-line
    /// human-readable summary; full output lives in the transcript.
    ToolCallExecuted {
        /// Configured name of the role that ran the tool.
        role: String,
        /// Engine-issued tool-use id matching a prior `ToolCallProposed`.
        tool_use_id: String,
        /// Whether the tool exited successfully.
        ok: bool,
        /// One-line summary of the tool output (truncated to a reasonable
        /// width); full output is captured in the transcript JSONL.
        output_summary: String,
    },
    /// Wrapper denied a proposed tool call via the PreToolUse hook.
    /// The tool did not run.
    PermissionDenied {
        /// Configured name of the role whose tool call was denied.
        role: String,
        /// Engine-native tool identifier of the denied call.
        tool_name: String,
        /// Engine-native tool input that triggered the deny.
        tool_input: serde_json::Value,
        /// Human-readable reason from the hook.
        reason: String,
    },
    /// Role subprocess exited. Final event emitted for the role's
    /// session id — any subsequent activity comes from a re-instantiated
    /// session with a new `session_id`.
    RoleStopped {
        /// Configured name of the role that stopped.
        role: String,
        /// Why the role stopped.
        reason: StopReason,
    },
}

/// Why a role stopped.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Subprocess exited cleanly after finishing its work.
    Completed,
    /// User invoked `/refresh` — the role will be re-instantiated with
    /// a fresh subprocess from priors + patches + transcript summary.
    Refreshed,
    /// Subprocess crashed unexpectedly. Wrapper logs the cause.
    Crashed,
    /// `--max-budget-usd` ceiling was hit. Tool calls and replies stop;
    /// user must explicitly raise the cap or `/refresh`.
    Budget,
    /// The wrapper timed out while waiting for the role to finish its
    /// current turn.
    TimedOut,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn role_started_roundtrips() {
        let event = CrepEvent::RoleStarted {
            role: "backend".into(),
            engine: "cc".into(),
            model: "claude-opus-4-7".into(),
            session_id: "abc-123".into(),
            priors_hash: "sha256:deadbeef".into(),
        };
        let wire = serde_json::to_string(&event).unwrap();
        let parsed: CrepEvent = serde_json::from_str(&wire).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn role_spoke_carries_mentions_and_cost() {
        let event = CrepEvent::RoleSpoke {
            role: "backend".into(),
            text: "Will check with @security and @frontend.".into(),
            mentions: vec!["security".into(), "frontend".into()],
            cost_usd: 0.0625,
            cache_read: 17_889,
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], "role_spoke");
        assert_eq!(wire["mentions"], json!(["security", "frontend"]));
        let parsed: CrepEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn work_title_roundtrips() {
        let event = CrepEvent::WorkTitle {
            role: "backend".into(),
            title: "Review adapter timeout paths".into(),
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], "work_title");
        assert_eq!(wire["title"], "Review adapter timeout paths");
        let parsed: CrepEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn tool_call_proposed_preserves_arbitrary_input() {
        let event = CrepEvent::ToolCallProposed {
            role: "backend".into(),
            tool_name: "Bash".into(),
            tool_input: json!({"command": "ls", "description": "list files"}),
            tool_use_id: "toolu_01abc".into(),
        };
        let wire = serde_json::to_string(&event).unwrap();
        let parsed: CrepEvent = serde_json::from_str(&wire).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn permission_denied_preserves_reason() {
        let event = CrepEvent::PermissionDenied {
            role: "backend".into(),
            tool_name: "Bash".into(),
            tool_input: json!({"command": "rm -rf /"}),
            reason: "destructive shell ops are denied by hook".into(),
        };
        let wire = serde_json::to_string(&event).unwrap();
        let parsed: CrepEvent = serde_json::from_str(&wire).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn stop_reason_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&StopReason::Refreshed).unwrap(),
            "\"refreshed\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::Budget).unwrap(),
            "\"budget\""
        );
        assert_eq!(
            serde_json::from_str::<StopReason>("\"completed\"").unwrap(),
            StopReason::Completed
        );
        assert_eq!(
            serde_json::from_str::<StopReason>("\"timed_out\"").unwrap(),
            StopReason::TimedOut
        );
    }

    #[test]
    fn role_stopped_event_shape() {
        let event = CrepEvent::RoleStopped {
            role: "backend".into(),
            reason: StopReason::Budget,
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], "role_stopped");
        assert_eq!(wire["reason"], "budget");
    }

    #[test]
    fn type_tag_is_snake_case_for_all_variants() {
        let cases = [
            (
                CrepEvent::RoleStarted {
                    role: "r".into(),
                    engine: "cc".into(),
                    model: "m".into(),
                    session_id: "s".into(),
                    priors_hash: "p".into(),
                },
                "role_started",
            ),
            (
                CrepEvent::WorkTitle {
                    role: "r".into(),
                    title: "t".into(),
                },
                "work_title",
            ),
            (
                CrepEvent::RoleSpoke {
                    role: "r".into(),
                    text: "t".into(),
                    mentions: vec![],
                    cost_usd: 0.0,
                    cache_read: 0,
                },
                "role_spoke",
            ),
            (
                CrepEvent::ToolCallProposed {
                    role: "r".into(),
                    tool_name: "Bash".into(),
                    tool_input: json!({}),
                    tool_use_id: "id".into(),
                },
                "tool_call_proposed",
            ),
            (
                CrepEvent::ToolCallExecuted {
                    role: "r".into(),
                    tool_use_id: "id".into(),
                    ok: true,
                    output_summary: String::new(),
                },
                "tool_call_executed",
            ),
            (
                CrepEvent::PermissionDenied {
                    role: "r".into(),
                    tool_name: "Bash".into(),
                    tool_input: json!({}),
                    reason: "no".into(),
                },
                "permission_denied",
            ),
            (
                CrepEvent::RoleStopped {
                    role: "r".into(),
                    reason: StopReason::Completed,
                },
                "role_stopped",
            ),
        ];
        for (event, expected_tag) in cases {
            let wire = serde_json::to_value(&event).unwrap();
            assert_eq!(wire["type"], expected_tag, "variant: {event:?}");
        }
    }
}
