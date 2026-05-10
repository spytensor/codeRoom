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
    // @foo.bar — match stops at the dot, but email local-parts do not route.
    let text = "send to user@example.com and ping @ops!";
    assert_eq!(parse_mentions(text), vec!["ops".to_owned()]);
}

#[test]
fn work_title_dedupe_keeps_first_title_per_turn() {
    let mut seen = false;
    assert!(matches!(
        dedupe_work_title_for_turn(
            CrepEvent::WorkTitle {
                role: "security".into(),
                title: "Scan permissions".into(),
            },
            &mut seen,
        ),
        Some(CrepEvent::WorkTitle { .. })
    ));
    assert!(dedupe_work_title_for_turn(
        CrepEvent::WorkTitle {
            role: "security".into(),
            title: "Scan permissions".into(),
        },
        &mut seen,
    )
    .is_none());
    assert!(matches!(
        dedupe_work_title_for_turn(
            CrepEvent::RoleSpoke {
                role: "security".into(),
                text: "done".into(),
                mentions: vec![],
                cost_usd: 0.0,
                cache_read: 0,
            },
            &mut seen,
        ),
        Some(CrepEvent::RoleSpoke { .. })
    ));
    seen = false;
    assert!(matches!(
        dedupe_work_title_for_turn(
            CrepEvent::WorkTitle {
                role: "security".into(),
                title: "Next turn".into(),
            },
            &mut seen,
        ),
        Some(CrepEvent::WorkTitle { title, .. }) if title == "Next turn"
    ));
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
fn translate_result_yields_permission_denied_events() {
    let line = json!({
        "type": "result",
        "subtype": "success",
        "result": "I could not run that command.",
        "permission_denials": [
            {
                "tool_name": "Bash",
                "tool_input": {"command": "rm -rf target"},
                "reason": "destructive shell ops require review"
            }
        ]
    });
    let events = translate("backend", "dh1:0", &line);
    assert_eq!(events.len(), 2);
    match &events[0] {
        CrepEvent::PermissionDenied {
            role,
            tool_name,
            tool_input,
            reason,
        } => {
            assert_eq!(role, "backend");
            assert_eq!(tool_name, "Bash");
            assert_eq!(tool_input["command"], "rm -rf target");
            assert_eq!(reason, "destructive shell ops require review");
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
    assert!(matches!(events[1], CrepEvent::RoleSpoke { .. }));
}

#[test]
fn hook_settings_points_at_hidden_hook_command() {
    let tmp = tempfile::tempdir().unwrap();
    let policy_path = tmp.path().join("permission_policy.json");
    let file = claude_hook_settings(PermissionMode::Auto, Some(&policy_path)).unwrap();
    let text = std::fs::read_to_string(file.path()).unwrap();
    assert!(text.contains("__coderoom-hook-decision"));
    assert!(text.contains("--mode auto"));
    assert!(text.contains("--policy-file"));
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
fn translate_assistant_text_yields_work_title_before_tool_use() {
    let line = json!({
        "type": "assistant",
        "message": {
            "content": [
                {"type": "text", "text": "```cr-task\nInspect permissions\n```"},
                {
                    "type": "tool_use",
                    "id": "toolu_01abc",
                    "name": "Read",
                    "input": {"file_path": "README.md"}
                }
            ]
        }
    });
    let events = translate("security", "h", &line);
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0],
        CrepEvent::WorkTitle {
            role: "security".into(),
            title: "Inspect permissions".into(),
        }
    );
    assert!(matches!(
        events[1],
        CrepEvent::ToolCallProposed {
            ref role,
            ref tool_name,
            ..
        } if role == "security" && tool_name == "Read"
    ));
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

#[tokio::test]
async fn write_stdin_waits_for_turn_boundary_before_next_prompt() {
    let (client, server) = tokio::io::duplex(4096);
    let mut lines = BufReader::new(client).lines();
    let (tx_user, rx_user) = mpsc::channel(4);
    let (turn_done_tx, turn_done_rx) = mpsc::channel(4);

    tokio::spawn(write_stdin(
        "backend".to_owned(),
        rx_user,
        server,
        turn_done_rx,
    ));

    tx_user
        .send(UserMessage::Prompt("first".into()))
        .await
        .unwrap();
    tx_user
        .send(UserMessage::Prompt("second".into()))
        .await
        .unwrap();

    let first = lines.next_line().await.unwrap().unwrap();
    assert!(first.contains("first"));

    let second_before_boundary =
        tokio::time::timeout(Duration::from_millis(50), lines.next_line()).await;
    assert!(
        second_before_boundary.is_err(),
        "second prompt was written before a turn boundary"
    );

    turn_done_tx.send(()).await.unwrap();
    let second = lines.next_line().await.unwrap().unwrap();
    assert!(second.contains("second"));
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
