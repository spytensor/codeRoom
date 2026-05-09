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
        role: String,
        engine: String,
        model: String,
        session_id: String,
        priors_hash: String,
    },
    /// The role emitted a final assistant turn (the LLM finished its
    /// response for the current user message).
    ///
    /// `mentions` is the list of `@<name>` references parsed out of the
    /// reply text; the wrapper uses this to route briefs to other roles.
    RoleSpoke {
        role: String,
        text: String,
        mentions: Vec<String>,
        cost_usd: f64,
        cache_read: u64,
    },
    /// Engine asked to call a tool; the wrapper's PreToolUse hook saw it
    /// before the tool ran. May be followed by either `ToolCallExecuted`
    /// (if approved) or `PermissionDenied` (if vetoed).
    ToolCallProposed {
        role: String,
        tool_name: String,
        tool_input: serde_json::Value,
        tool_use_id: String,
    },
    /// Tool call ran to completion. `output_summary` is a one-line
    /// human-readable summary; full output lives in the transcript.
    ToolCallExecuted {
        role: String,
        tool_use_id: String,
        ok: bool,
        output_summary: String,
    },
    /// Wrapper denied a proposed tool call via the PreToolUse hook.
    /// The tool did not run.
    PermissionDenied {
        role: String,
        tool_name: String,
        tool_input: serde_json::Value,
        reason: String,
    },
    /// Role subprocess exited. Final event emitted for the role's
    /// session id — any subsequent activity comes from a re-instantiated
    /// session with a new `session_id`.
    RoleStopped { role: String, reason: StopReason },
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
        assert_eq!(serde_json::to_string(&StopReason::Refreshed).unwrap(), "\"refreshed\"");
        assert_eq!(serde_json::to_string(&StopReason::Budget).unwrap(), "\"budget\"");
        assert_eq!(
            serde_json::from_str::<StopReason>("\"completed\"").unwrap(),
            StopReason::Completed
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
                    output_summary: "".into(),
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
