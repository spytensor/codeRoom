//! CodeRoom Event Protocol — the normalized event stream emitted by every
//! engine adapter and consumed by the message bus, REPL, and patch logic.
//!
//! See `docs/architecture.md` § "CodeRoom Event Protocol (CREP)" and
//! `docs/v0.2-trust-and-interrupt.md` § D for the v0.2 amendment.
//!
//! Wire format is JSON: each event serializes to a single object with a
//! `"type"` discriminator and snake_case field names. The append-only
//! `.coderoom/messages.jsonl` log stores durable events in this exact
//! shape; live-only deltas may be broadcast without being appended.
//!
//! ### v0.2 turn-id and thread-id
//!
//! Every turn-scoped event carries `turn_id` (this turn) and `thread_id`
//! (the conversation chain that survives auto-routing). Both default to
//! the empty string [`crate::turn::LEGACY_TURN_ID`] when missing, so
//! v0.1-shaped logs replay without modification. New CREP producers
//! should always populate them via [`crate::turn::new_turn_id`] and
//! [`crate::turn::new_thread_id`].

use serde::{Deserialize, Serialize};

use crate::turn::TurnId;

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
    /// A new turn was dispatched to a role. Emitted by the REPL (or the
    /// auto-router) before the role's adapter starts producing events,
    /// so `cr show` and the renderer can mark queued state visibly.
    ///
    /// `parent_turn_id` is set when this turn was triggered by another
    /// role's explicit `@<peer>` delegation (auto-routed);
    /// `queue_position` reports
    /// where this turn sits in the role's per-role queue at dispatch
    /// time (0 = will start immediately, 1 = one turn ahead, …).
    TurnDispatched {
        /// Configured name of the dispatched role.
        role: String,
        /// Opaque turn id, also carried on every later turn-scoped event.
        turn_id: TurnId,
        /// Opaque conversation-thread id; preserved across auto-routed
        /// hops within a single chain.
        thread_id: TurnId,
        /// `turn_id` of the role-turn that auto-routed to this one, when
        /// applicable. `None` for user-originated dispatches.
        parent_turn_id: Option<TurnId>,
        /// Queue depth ahead of this turn at dispatch time.
        queue_position: usize,
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
        /// Opaque turn id this title belongs to. Empty string for v0.1
        /// log replay.
        #[serde(default)]
        turn_id: TurnId,
        /// Opaque conversation-thread id. Empty string for v0.1 log replay.
        #[serde(default)]
        thread_id: TurnId,
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
        /// Opaque turn id this reply belongs to. Empty string for v0.1
        /// log replay.
        #[serde(default)]
        turn_id: TurnId,
        /// Opaque conversation-thread id. Empty string for v0.1 log replay.
        #[serde(default)]
        thread_id: TurnId,
    },
    /// Streaming assistant text for the current role turn. This is a
    /// broadcast-only live UI event, not a turn boundary, and must not
    /// drive auto-routing; `RoleSpoke` remains the authoritative final
    /// text.
    RoleOutputDelta {
        /// Configured name of the role that emitted text.
        role: String,
        /// Append-only text chunk emitted by the engine.
        text_delta: String,
        /// Monotonic sequence number within the role adapter's current turn.
        sequence: u64,
        /// Opaque turn id this delta belongs to. Empty string for legacy paths.
        #[serde(default)]
        turn_id: TurnId,
        /// Opaque conversation-thread id. Empty string for legacy paths.
        #[serde(default)]
        thread_id: TurnId,
    },
    /// Turn was interrupted before the role produced a final reply.
    /// Distinct from `RoleStopped` — the role process is still alive
    /// and reachable for the next dispatch.
    ///
    /// `partial_text` carries whatever the engine had emitted for this
    /// turn before cancellation (best-effort per adapter); mentions
    /// parsed from a partial reply are surfaced for the user but
    /// **never auto-routed** (a half-thought shouldn't cascade).
    TurnInterrupted {
        /// Configured name of the interrupted role.
        role: String,
        /// Opaque turn id matching the dispatched turn.
        turn_id: TurnId,
        /// Opaque conversation-thread id of the interrupted turn.
        thread_id: TurnId,
        /// What requested the interruption.
        source: InterruptSource,
        /// Best-effort engine output captured before cancellation.
        partial_text: Option<String>,
        /// `@<name>` references parsed from `partial_text`, surfaced
        /// for the user but not auto-routed.
        partial_mentions: Vec<String>,
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
        /// Opaque turn id this tool call belongs to. Empty string for
        /// v0.1 log replay.
        #[serde(default)]
        turn_id: TurnId,
        /// Opaque conversation-thread id. Empty string for v0.1 log replay.
        #[serde(default)]
        thread_id: TurnId,
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
        /// Opaque turn id this tool call belongs to. Empty string for
        /// v0.1 log replay.
        #[serde(default)]
        turn_id: TurnId,
        /// Opaque conversation-thread id. Empty string for v0.1 log replay.
        #[serde(default)]
        thread_id: TurnId,
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
        /// Opaque turn id this denial belongs to. Empty string for
        /// v0.1 log replay.
        #[serde(default)]
        turn_id: TurnId,
        /// Opaque conversation-thread id. Empty string for v0.1 log replay.
        #[serde(default)]
        thread_id: TurnId,
    },
    /// Role subprocess exited. Final event emitted for the role's
    /// session id — any subsequent activity comes from a re-instantiated
    /// session with a new `session_id`.
    RoleStopped {
        /// Configured name of the role that stopped.
        role: String,
        /// Why the role stopped.
        reason: StopReason,
        /// Set when the role stopped while a specific turn was in
        /// flight (crash mid-turn, refresh during a turn, …); used to
        /// finalize the right WorkCard. `None` for clean
        /// between-turn stops. v0.1 log replay also lands here as `None`.
        ///
        /// Skipped on serialize when `None` so v0.1 log readers (or
        /// any downstream tool with `deny_unknown_fields`) don't see
        /// an unfamiliar `null` field appear out of nowhere; the v0.2
        /// JSONL log only carries `turn_id` when it has something
        /// real to say.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
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
    ///
    /// Retired from the v0.2 wall-clock path: the REPL's
    /// `PER_TURN_TIMEOUT` is going away (see PR b).
    /// Kept on the wire so v0.1 logs still deserialize.
    TimedOut,
}

/// Why a turn was interrupted. Distinct sources are kept separate so
/// renderers and `cr show` can attribute cleanly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InterruptSource {
    /// User typed `/halt` or `/halt @role`.
    UserHalt,
    /// User pressed Ctrl-C once (first-press cancels in-flight turns).
    UserCtrlC,
    /// Per-adapter stdio idle watchdog fired (engine emitted no events
    /// for the configured idle window — see
    /// `docs/v0.2-trust-and-interrupt.md` § B).
    WatchdogIdle,
    /// `cancel_turn` did not produce a turn-final event within the 5s
    /// SLO; adapter escalated to a process kill via `stop_tx`.
    CancelTimeout,
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
            turn_id: "t-1".into(),
            thread_id: "th-1".into(),
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], "role_spoke");
        assert_eq!(wire["mentions"], json!(["security", "frontend"]));
        assert_eq!(wire["turn_id"], "t-1");
        assert_eq!(wire["thread_id"], "th-1");
        let parsed: CrepEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn role_spoke_legacy_log_replay_tolerates_missing_turn_ids() {
        // v0.1-shaped messages.jsonl has no turn_id / thread_id;
        // deserialization must still succeed and fall back to empty.
        let legacy = json!({
            "type": "role_spoke",
            "role": "backend",
            "text": "ok",
            "mentions": [],
            "cost_usd": 0.0,
            "cache_read": 0,
        });
        let parsed: CrepEvent = serde_json::from_value(legacy).unwrap();
        match parsed {
            CrepEvent::RoleSpoke {
                turn_id, thread_id, ..
            } => {
                assert_eq!(turn_id, "");
                assert_eq!(thread_id, "");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn role_stopped_with_in_flight_turn_round_trips() {
        let event = CrepEvent::RoleStopped {
            role: "backend".into(),
            reason: StopReason::Crashed,
            turn_id: Some("t-7".into()),
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["turn_id"], "t-7");
        let parsed: CrepEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn role_stopped_legacy_log_replay_tolerates_missing_turn_id() {
        let legacy = json!({
            "type": "role_stopped",
            "role": "backend",
            "reason": "completed",
        });
        let parsed: CrepEvent = serde_json::from_value(legacy).unwrap();
        match parsed {
            CrepEvent::RoleStopped { turn_id, .. } => assert!(turn_id.is_none()),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn turn_dispatched_event_shape() {
        let event = CrepEvent::TurnDispatched {
            role: "security".into(),
            turn_id: "t-2".into(),
            thread_id: "th-1".into(),
            parent_turn_id: Some("t-1".into()),
            queue_position: 1,
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], "turn_dispatched");
        assert_eq!(wire["parent_turn_id"], "t-1");
        assert_eq!(wire["queue_position"], 1);
        let parsed: CrepEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn turn_interrupted_carries_partial_payload() {
        let event = CrepEvent::TurnInterrupted {
            role: "security".into(),
            turn_id: "t-3".into(),
            thread_id: "th-2".into(),
            source: InterruptSource::UserHalt,
            partial_text: Some("partial reply mentioning @backend".into()),
            partial_mentions: vec!["backend".into()],
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], "turn_interrupted");
        assert_eq!(wire["source"], "user_halt");
        assert_eq!(wire["partial_mentions"], json!(["backend"]));
        let parsed: CrepEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn interrupt_source_serializes_snake_case() {
        for (variant, expected) in [
            (InterruptSource::UserHalt, "user_halt"),
            (InterruptSource::UserCtrlC, "user_ctrl_c"),
            (InterruptSource::WatchdogIdle, "watchdog_idle"),
            (InterruptSource::CancelTimeout, "cancel_timeout"),
        ] {
            assert_eq!(
                serde_json::to_string(&variant).unwrap(),
                format!("\"{expected}\""),
                "variant {variant:?}"
            );
        }
    }

    #[test]
    fn turn_interrupted_round_trips_for_each_source() {
        // Lock down the full TurnInterrupted envelope for every source
        // variant. Catches both serialize order and `partial_text` /
        // `partial_mentions` defaults.
        for source in [
            InterruptSource::UserHalt,
            InterruptSource::UserCtrlC,
            InterruptSource::WatchdogIdle,
            InterruptSource::CancelTimeout,
        ] {
            let event = CrepEvent::TurnInterrupted {
                role: "security".into(),
                turn_id: "tu-1".into(),
                thread_id: "th-1".into(),
                source,
                partial_text: None,
                partial_mentions: vec![],
            };
            let wire = serde_json::to_value(&event).unwrap();
            assert_eq!(wire["type"], "turn_interrupted", "source {source:?}");
            assert_eq!(
                wire["source"].as_str().unwrap(),
                match source {
                    InterruptSource::UserHalt => "user_halt",
                    InterruptSource::UserCtrlC => "user_ctrl_c",
                    InterruptSource::WatchdogIdle => "watchdog_idle",
                    InterruptSource::CancelTimeout => "cancel_timeout",
                }
            );
            let parsed: CrepEvent = serde_json::from_value(wire).unwrap();
            assert_eq!(event, parsed, "source {source:?}");
        }
    }

    #[test]
    fn turn_dispatched_chain_preserves_thread_id_across_hops() {
        // Auto-routed chain T1 → T2 → T3 on one thread. parent_turn_id
        // ancestry plus a single shared thread_id is the v0.2 contract
        // replay and future parallel fan-out rely on; lock the shape now
        // so a future refactor doesn't quietly drop a field.
        let chain = [
            CrepEvent::TurnDispatched {
                role: "host".into(),
                turn_id: "tu-1".into(),
                thread_id: "th-7".into(),
                parent_turn_id: None,
                queue_position: 0,
            },
            CrepEvent::TurnDispatched {
                role: "backend".into(),
                turn_id: "tu-2".into(),
                thread_id: "th-7".into(),
                parent_turn_id: Some("tu-1".into()),
                queue_position: 0,
            },
            CrepEvent::TurnDispatched {
                role: "security".into(),
                turn_id: "tu-3".into(),
                thread_id: "th-7".into(),
                parent_turn_id: Some("tu-2".into()),
                queue_position: 0,
            },
        ];

        let wire: Vec<_> = chain
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        // Shared thread_id across the chain.
        for event in &wire {
            assert_eq!(event["thread_id"], "th-7");
        }
        // Each child points to its parent.
        assert_eq!(wire[1]["parent_turn_id"], "tu-1");
        assert_eq!(wire[2]["parent_turn_id"], "tu-2");
        // The root has no parent.
        assert!(wire[0]["parent_turn_id"].is_null());
        // Round-trip every line.
        for (event, line) in chain.iter().zip(wire.iter()) {
            let parsed: CrepEvent = serde_json::from_value(line.clone()).unwrap();
            assert_eq!(event, &parsed);
        }
    }

    #[test]
    fn role_stopped_skips_turn_id_when_none_on_serialize() {
        // Forward-compat: v0.1 readers must not see a `turn_id: null`
        // field appear on every RoleStopped line. The Option is
        // skipped on serialize when None.
        let event = CrepEvent::RoleStopped {
            role: "backend".into(),
            reason: StopReason::Completed,
            turn_id: None,
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert!(
            wire.get("turn_id").is_none(),
            "expected turn_id to be skipped, got: {wire}"
        );
        // Round-trip still works.
        let parsed: CrepEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn work_title_roundtrips() {
        let event = CrepEvent::WorkTitle {
            role: "backend".into(),
            title: "Review adapter timeout paths".into(),
            turn_id: "t-1".into(),
            thread_id: "th-1".into(),
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
            turn_id: "t-1".into(),
            thread_id: "th-1".into(),
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
            turn_id: "t-1".into(),
            thread_id: "th-1".into(),
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
            turn_id: None,
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], "role_stopped");
        assert_eq!(wire["reason"], "budget");
    }

    #[test]
    fn type_tag_is_snake_case_for_all_variants() {
        let cases: [(CrepEvent, &str); 10] = [
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
                CrepEvent::TurnDispatched {
                    role: "r".into(),
                    turn_id: "t-1".into(),
                    thread_id: "th-1".into(),
                    parent_turn_id: None,
                    queue_position: 0,
                },
                "turn_dispatched",
            ),
            (
                CrepEvent::WorkTitle {
                    role: "r".into(),
                    title: "t".into(),
                    turn_id: "t-1".into(),
                    thread_id: "th-1".into(),
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
                    turn_id: "t-1".into(),
                    thread_id: "th-1".into(),
                },
                "role_spoke",
            ),
            (
                CrepEvent::RoleOutputDelta {
                    role: "r".into(),
                    text_delta: "chunk".into(),
                    sequence: 1,
                    turn_id: "t-1".into(),
                    thread_id: "th-1".into(),
                },
                "role_output_delta",
            ),
            (
                CrepEvent::TurnInterrupted {
                    role: "r".into(),
                    turn_id: "t-1".into(),
                    thread_id: "th-1".into(),
                    source: InterruptSource::UserHalt,
                    partial_text: None,
                    partial_mentions: vec![],
                },
                "turn_interrupted",
            ),
            (
                CrepEvent::ToolCallProposed {
                    role: "r".into(),
                    tool_name: "Bash".into(),
                    tool_input: json!({}),
                    tool_use_id: "id".into(),
                    turn_id: "t-1".into(),
                    thread_id: "th-1".into(),
                },
                "tool_call_proposed",
            ),
            (
                CrepEvent::ToolCallExecuted {
                    role: "r".into(),
                    tool_use_id: "id".into(),
                    ok: true,
                    output_summary: String::new(),
                    turn_id: "t-1".into(),
                    thread_id: "th-1".into(),
                },
                "tool_call_executed",
            ),
            (
                CrepEvent::PermissionDenied {
                    role: "r".into(),
                    tool_name: "Bash".into(),
                    tool_input: json!({}),
                    reason: "no".into(),
                    turn_id: "t-1".into(),
                    thread_id: "th-1".into(),
                },
                "permission_denied",
            ),
            (
                CrepEvent::RoleStopped {
                    role: "r".into(),
                    reason: StopReason::Completed,
                    turn_id: None,
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
