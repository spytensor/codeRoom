use std::collections::BTreeMap;

use super::render::{render_event_line, started_model_label, summarize_tool_input};
use super::show::filter_show_events;
use super::splash::{
    join_cells, load_splash_content, pick_release, plain_cell, render_home_at_width, splash_bottom,
    splash_columns, splash_pair, splash_top, styled_cell,
};
use super::status::{StatusRegion, StatusSlot, SPINNER_FRAMES};
use super::text::truncate_inline;
use super::turn::TurnActivity;
use super::*;
use pretty_assertions::assert_eq;

#[test]
fn parse_empty_input_yields_empty_command() {
    assert_eq!(parse_line(""), Command::Empty);
    assert_eq!(parse_line("   "), Command::Empty);
    assert_eq!(parse_line("\n"), Command::Empty);
}

#[test]
fn parse_at_mention_routes_to_role() {
    match parse_line("@backend please read the auth module") {
        Command::SendTo { role, text } => {
            assert_eq!(role, "backend");
            assert_eq!(text, "please read the auth module");
        }
        other => panic!("expected SendTo, got {other:?}"),
    }
}

#[test]
fn parse_at_all_broadcasts() {
    match parse_line("@all summarize blockers") {
        Command::Broadcast(text) => assert_eq!(text, "summarize blockers"),
        other => panic!("expected Broadcast, got {other:?}"),
    }
}

#[test]
fn show_filter_keeps_role_events_and_applies_tail() {
    let events = vec![
        CrepEvent::RoleStarted {
            role: "backend".into(),
            engine: "cc".into(),
            model: "claude".into(),
            session_id: "s1".into(),
            priors_hash: "p".into(),
        },
        CrepEvent::RoleSpoke {
            role: "security".into(),
            text: "not this role".into(),
            mentions: vec![],
            cost_usd: 0.0,
            cache_read: 0,
        },
        CrepEvent::ToolCallProposed {
            role: "backend".into(),
            tool_name: "Read".into(),
            tool_input: serde_json::json!({"file_path": "README.md"}),
            tool_use_id: "tool-1".into(),
        },
        CrepEvent::ToolCallExecuted {
            role: "backend".into(),
            tool_use_id: "tool-1".into(),
            ok: true,
            output_summary: "README.md".into(),
        },
    ];
    let options = ShowOptions {
        role: Some("backend".into()),
        since: None,
        tail: Some(2),
    };

    let filtered = filter_show_events(&events, &options);

    assert_eq!(filtered.len(), 2);
    assert!(matches!(filtered[0], CrepEvent::ToolCallProposed { .. }));
    assert!(matches!(filtered[1], CrepEvent::ToolCallExecuted { .. }));
}

#[test]
fn show_filter_tail_zero_renders_no_events() {
    let events = vec![CrepEvent::RoleStopped {
        role: "backend".into(),
        reason: StopReason::Completed,
    }];
    let options = ShowOptions {
        role: None,
        since: None,
        tail: Some(0),
    };

    assert!(filter_show_events(&events, &options).is_empty());
}

#[test]
fn parse_bare_text_routes_to_host() {
    match parse_line("any free-form text here") {
        Command::SendToHost(text) => assert_eq!(text, "any free-form text here"),
        other => panic!("expected SendToHost, got {other:?}"),
    }
}

#[test]
fn parse_at_with_no_text_falls_back_to_host() {
    // "@backend" alone is treated as bare text, not a routing command,
    // because there's nothing to send. The host gets the literal "@backend".
    match parse_line("@backend") {
        Command::SendToHost(text) => assert_eq!(text, "@backend"),
        other => panic!("expected SendToHost, got {other:?}"),
    }
}

#[test]
fn parse_slash_exit_quit_help() {
    assert_eq!(parse_line("/exit"), Command::Exit);
    assert_eq!(parse_line("/quit"), Command::Exit);
    assert_eq!(parse_line("/help"), Command::Help);
    assert_eq!(parse_line("/h"), Command::Help);
}

#[test]
fn parse_slash_stop_with_role() {
    assert_eq!(parse_line("/stop backend"), Command::Stop("backend".into()));
    assert_eq!(
        parse_line("/stop @backend"),
        Command::Stop("backend".into())
    );
}

#[test]
fn parse_slash_host_with_role() {
    assert_eq!(parse_line("/host backend"), Command::Host("backend".into()));
    assert_eq!(
        parse_line("/host @backend"),
        Command::Host("backend".into())
    );
}

#[test]
fn parse_slash_stop_without_role_shows_help() {
    // Defensively show help rather than tearing down something arbitrary.
    assert_eq!(parse_line("/stop"), Command::Help);
}

#[test]
fn parse_unknown_slash_shows_help() {
    assert_eq!(parse_line("/whatever"), Command::Help);
}

#[test]
fn parse_patch_with_role_and_text() {
    assert_eq!(
        parse_line("/patch backend rate limit goes in gateway config"),
        Command::Patch {
            role: "backend".into(),
            text: "rate limit goes in gateway config".into(),
        }
    );
}

#[test]
fn parse_patch_accepts_at_prefixed_role() {
    assert_eq!(
        parse_line("/patch @backend use verify_token()"),
        Command::Patch {
            role: "backend".into(),
            text: "use verify_token()".into(),
        }
    );
}

#[test]
fn parse_patch_without_text_shows_help() {
    assert_eq!(parse_line("/patch backend"), Command::Help);
}

#[test]
fn parse_patch_without_role_shows_help() {
    assert_eq!(parse_line("/patch"), Command::Help);
}

#[test]
fn parse_refresh_with_role() {
    assert_eq!(
        parse_line("/refresh backend"),
        Command::Refresh("backend".into())
    );
}

#[test]
fn parse_refresh_accepts_at_prefixed_role() {
    assert_eq!(
        parse_line("/refresh @backend"),
        Command::Refresh("backend".into())
    );
}

#[test]
fn parse_refresh_without_role_shows_help() {
    assert_eq!(parse_line("/refresh"), Command::Help);
}

#[test]
fn parse_transcript_with_role() {
    assert_eq!(
        parse_line("/transcript backend"),
        Command::Transcript("backend".into())
    );
    assert_eq!(
        parse_line("/transcript @backend"),
        Command::Transcript("backend".into())
    );
}

#[test]
fn parse_transcript_without_role_shows_help() {
    assert_eq!(parse_line("/transcript"), Command::Help);
}

#[test]
fn parse_journal_with_role() {
    assert_eq!(
        parse_line("/journal backend"),
        Command::Journal("backend".into())
    );
    assert_eq!(
        parse_line("/journal @backend"),
        Command::Journal("backend".into())
    );
}

#[test]
fn parse_journal_without_role_shows_help() {
    assert_eq!(parse_line("/journal"), Command::Help);
}

#[test]
fn parse_welcome() {
    assert_eq!(parse_line("/welcome"), Command::Welcome);
}

#[test]
fn parse_allow_and_deny() {
    assert_eq!(parse_line("/allow Read"), Command::Allow("Read".into()));
    assert_eq!(parse_line("/deny Bash"), Command::Deny("Bash".into()));
    assert_eq!(parse_line("/allow"), Command::Help);
    assert_eq!(parse_line("/deny"), Command::Help);
}

#[test]
fn started_model_label_hides_literal_model_placeholder() {
    assert_eq!(started_model_label("codex", "model"), "Codex default");
    assert_eq!(
        started_model_label("cc", "claude-opus-4-7"),
        "claude-opus-4-7"
    );
}

#[tokio::test]
async fn first_run_marker_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let coderoom = tmp.path().to_path_buf();
    // No marker yet → first run
    assert!(is_first_run(&coderoom));
    mark_welcomed(&coderoom).await;
    // Marker present → not first run
    assert!(!is_first_run(&coderoom));
    // Idempotent: second mark is a no-op (same path, same content)
    mark_welcomed(&coderoom).await;
    assert!(!is_first_run(&coderoom));
}

#[test]
fn ensure_permission_policy_creates_file_and_gitignore_rule() {
    let tmp = tempfile::tempdir().unwrap();
    let coderoom = tmp.path().join(CODEROOM_DIR);
    std::fs::create_dir_all(&coderoom).unwrap();
    std::fs::write(coderoom.join(".gitignore"), "messages.jsonl\n").unwrap();
    let policy_path = coderoom.join("permission_policy.json");

    ensure_permission_policy(&policy_path).unwrap();
    ensure_permission_policy(&policy_path).unwrap();

    assert!(policy_path.is_file());
    let ignore = std::fs::read_to_string(coderoom.join(".gitignore")).unwrap();
    assert!(ignore.contains("messages.jsonl"));
    assert_eq!(
        ignore
            .lines()
            .filter(|line| line.trim() == "permission_policy.json")
            .count(),
        1
    );
}

#[test]
fn splash_content_toml_parses_and_has_required_shape() {
    let content = load_splash_content();
    assert!(
        !content.tips.items.is_empty(),
        "tips.items must list at least one entry"
    );
    assert!(
        !content.whats_new.is_empty(),
        "whats_new must list at least one release entry"
    );
    for release in &content.whats_new {
        assert!(
            !release.version.is_empty(),
            "every [[whats_new]] entry needs a version"
        );
        assert!(
            !release.items.is_empty(),
            "release {} needs at least one item",
            release.version
        );
    }
}

#[test]
fn splash_pick_release_prefers_exact_version_match() {
    let content = load_splash_content();
    // Pick something we know exists in the bundled file.
    let head = content.whats_new.first().expect("at least one release");
    let picked = pick_release(&content, &head.version).expect("exact match");
    assert_eq!(picked.version, head.version);
}

#[test]
fn splash_pick_release_falls_back_to_head_when_unknown() {
    let content = load_splash_content();
    let picked =
        pick_release(&content, "0.0.0-not-a-real-version").expect("fallback to head when no match");
    assert_eq!(picked.version, content.whats_new[0].version);
}

#[test]
fn splash_columns_keep_total_width_within_budget() {
    // For every reasonable width and role-floor, the row formula
    // must reproduce the requested width: 6 + left + right == width.
    for width in [60usize, 70, 80] {
        for floor in [0usize, 22, 28, 36] {
            let (left, right) = splash_columns(width, floor);
            assert_eq!(
                6 + left + right,
                width,
                "row formula broke at width={width}, floor={floor}"
            );
        }
    }
}

fn strip_ansi(s: &str) -> String {
    // Minimal CSI stripper — enough for crossterm's SGR sequences.
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn splash_top_and_bottom_have_exact_frame_width() {
    for width in [60usize, 70, 80] {
        let title = join_cells(&[
            styled_cell("codeRoom", "codeRoom".with(output::SPLASH_FRAME).bold()),
            plain_cell(" "),
            styled_cell("v9.9.9", "v9.9.9".with(output::SPLASH_VERSION)),
        ]);
        let top = strip_ansi(&splash_top(width, &title));
        let bottom = strip_ansi(&splash_bottom(width));
        assert_eq!(top.chars().count(), width, "top mismatched at {width}");
        assert_eq!(
            bottom.chars().count(),
            width,
            "bottom mismatched at {width}"
        );
        assert!(top.starts_with('┌'), "top must start with ┌");
        assert!(top.ends_with('┐'), "top must end with ┐");
        assert!(bottom.starts_with('└'), "bottom must start with └");
        assert!(bottom.ends_with('┘'), "bottom must end with ┘");
    }
}

#[test]
fn splash_pair_rows_align_at_every_width() {
    // Mix of empty cells, narrow cells, and wide-but-fits cells.
    let cases: &[(&str, &str)] = &[
        ("", ""),
        ("welcome back, charlie", "tips for getting started"),
        ("● @host  cc · 1M", "• send a task to @host"),
        ("[ 1.0k ] base tokens", "what's new in 0.1.17"),
    ];
    for width in [60usize, 70, 80] {
        let (left_w, right_w) = splash_columns(width, 28);
        for (l, r) in cases {
            let left = plain_cell(*l);
            let right = plain_cell(*r);
            let row = strip_ansi(&splash_pair(&left, &right, left_w, right_w));
            assert_eq!(
                row.chars().count(),
                width,
                "row width mismatch at {width} for ({l:?}, {r:?}): {row:?}"
            );
            assert!(row.starts_with('│'), "left edge must be │");
            assert!(row.ends_with('│'), "right edge must be │");
        }
    }
}

#[test]
fn snapshot_splash_frame_shape_at_80() {
    let (left_w, right_w) = splash_columns(80, 28);
    let title = join_cells(&[
        styled_cell("codeRoom", "codeRoom".with(output::SPLASH_FRAME).bold()),
        plain_cell(" "),
        styled_cell("v0.1.17", "v0.1.17".with(output::SPLASH_VERSION)),
    ]);
    let rendered = [
        strip_ansi(&splash_top(80, &title)),
        strip_ansi(&splash_pair(
            &plain_cell("welcome back, chao"),
            &plain_cell("tips for getting started"),
            left_w,
            right_w,
        )),
        strip_ansi(&splash_bottom(80)),
    ]
    .join("\n");
    insta::assert_snapshot!(rendered, @r"
┌─ codeRoom v0.1.17 ───────────────────────────────────────────────────────────┐
│ welcome back, chao             tips for getting started                      │
└──────────────────────────────────────────────────────────────────────────────┘");
}

fn splash_snapshot_config() -> Config {
    Config {
        default_engine: Engine::Cc,
        default_model: None,
        permission_mode: PermissionMode::Ask,
        budget_per_role_usd: 1.0,
        host_role: "host".into(),
        roles: HashMap::from([
            (
                "host".into(),
                crate::config::RoleEntry {
                    engine: Some(Engine::Cc),
                    model: Some("opus".into()),
                    permission_mode: None,
                },
            ),
            (
                "backend".into(),
                crate::config::RoleEntry {
                    engine: Some(Engine::Cc),
                    model: None,
                    permission_mode: None,
                },
            ),
            (
                "security".into(),
                crate::config::RoleEntry {
                    engine: Some(Engine::Codex),
                    model: None,
                    permission_mode: Some(PermissionMode::Bypass),
                },
            ),
        ]),
    }
}

#[test]
fn snapshot_boot_dashboard_at_80() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = splash_snapshot_config();
    let rendered = strip_ansi(&render_home_at_width(
        &cfg,
        tmp.path(),
        Path::new("/repo/codeRoom"),
        false,
        80,
        Some("Ada"),
    ))
    .trim_start_matches('\n')
    .to_owned();
    insta::assert_snapshot!(rendered, @r"
┌─ codeRoom v0.1.17 ───────────────────────────────────────────────────────────┐
│                                                                              │
│ welcome back, Ada              tips for getting started                      │
│                                • type @role to send a task to a specific ro… │
│ ● @backend   cc     · 1M       • /patch <role> persists a correction across… │
│ ● @host      cc     · 1M       • /journal <role> captures today's lessons-l… │
│ ● @security  codex  · default                                                │
│                                what's new in 0.1.17                          │
│  0  base tokens loaded         • Codex bypass disables both approvals and t… │
│ /repo/codeRoom                 • Codex timeouts clean up the spawned vendor… │
│                                • README screenshot now shows the multi-engi… │
│                                                                              │
│                                /release-notes for more                       │
│                                                                              │
└──────────────────────────────────────────────────────────────────────────────┘
  type a task · @role · /help · /exit
");
}

#[test]
fn snapshot_render_event_lines() {
    let events = [
        CrepEvent::RoleStarted {
            role: "backend".into(),
            engine: "cc".into(),
            model: "claude-opus-4-7".into(),
            session_id: "s".into(),
            priors_hash: "p".into(),
        },
        CrepEvent::WorkTitle {
            role: "backend".into(),
            title: "Review work cards".into(),
        },
        CrepEvent::RoleSpoke {
            role: "backend".into(),
            text: "Ready for @security.".into(),
            mentions: vec!["security".into()],
            cost_usd: 0.12,
            cache_read: 42,
        },
        CrepEvent::ToolCallProposed {
            role: "backend".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "cargo test --all-features"}),
            tool_use_id: "tool-1".into(),
        },
        CrepEvent::ToolCallExecuted {
            role: "backend".into(),
            tool_use_id: "tool-1".into(),
            ok: true,
            output_summary: "tests passed".into(),
        },
        CrepEvent::PermissionDenied {
            role: "backend".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "rm -rf target"}),
            reason: "destructive shell ops require review".into(),
        },
        CrepEvent::RoleStopped {
            role: "backend".into(),
            reason: StopReason::Refreshed,
        },
    ];
    let rendered = events
        .iter()
        .map(|event| strip_ansi(&render_event_line(event, "host")))
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r"
▎ @backend ready · model=claude-opus-4-7
▎ @backend work · Review work cards
▎ @backend Ready for @security.
▎ ↳ @backend · Bash `cargo test --all-features`
▎ ✓ @backend · tests passed
▎ ⊘ @backend · Bash denied: destructive shell ops require review
▎ @backend stopped: Refreshed
");
}

#[test]
fn multi_line_role_spoke_keeps_gutter_on_each_line() {
    let event = CrepEvent::RoleSpoke {
        role: "backend".into(),
        text: "First paragraph.\nSecond paragraph.\nThird paragraph.".into(),
        cost_usd: 0.0,
        cache_read: 0,
        mentions: vec![],
    };
    let rendered = strip_ansi(&render_event_line(&event, "host"));
    insta::assert_snapshot!(rendered, @r"
▎ @backend First paragraph.
▎ Second paragraph.
▎ Third paragraph.
");
}

#[test]
fn turn_activity_folds_tool_events() {
    let mut activity = TurnActivity::default();
    for event in [
        CrepEvent::ToolCallProposed {
            role: "host".into(),
            tool_name: "Read".into(),
            tool_input: serde_json::json!({"file_path": "README.md"}),
            tool_use_id: "1".into(),
        },
        CrepEvent::ToolCallProposed {
            role: "host".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            tool_use_id: "2".into(),
        },
        CrepEvent::ToolCallExecuted {
            role: "host".into(),
            tool_use_id: "1".into(),
            ok: true,
            output_summary: "README.md".into(),
        },
        CrepEvent::ToolCallExecuted {
            role: "host".into(),
            tool_use_id: "2".into(),
            ok: true,
            output_summary: "Cargo.toml".into(),
        },
    ] {
        TurnActivity::from_foldable_event(&event, "host")
            .expect("foldable")
            .merge_into(&mut activity);
    }

    assert_eq!(
        activity.summary_line("host").as_deref(),
        Some("  @host tools folded · Bash, Read · 2 ok")
    );
}

#[test]
fn turn_activity_ignores_other_roles() {
    let event = CrepEvent::ToolCallProposed {
        role: "security".into(),
        tool_name: "Read".into(),
        tool_input: serde_json::json!({"file_path": "README.md"}),
        tool_use_id: "1".into(),
    };

    assert!(TurnActivity::from_foldable_event(&event, "host").is_none());
}

#[test]
fn turn_activity_grounding_gate_fires_on_three_permission_denials() {
    let activity = TurnActivity {
        proposed: 3,
        completed: 0,
        failed: 0,
        denied: 3,
        tools: BTreeMap::new(),
        denied_tools: BTreeMap::from([("Read".to_string(), 2), ("Bash".to_string(), 1)]),
    };
    assert!(activity.looks_ungrounded());
}

#[test]
fn turn_activity_grounding_gate_quiet_on_legitimate_failures() {
    // Failing tests / rg with no matches / command exit-1 are NOT
    // ungrounded — those tools ran and produced output the role can
    // reason about. Only permission denials trip the gate.
    let activity = TurnActivity {
        proposed: 3,
        completed: 3,
        failed: 3,
        denied: 0,
        tools: BTreeMap::from([("Bash".to_string(), 3)]),
        denied_tools: BTreeMap::new(),
    };
    assert!(!activity.looks_ungrounded());
}

#[test]
fn turn_activity_grounding_gate_fires_when_all_proposed_were_blocked() {
    // 2 proposed, 2 denied → no successful execution → gate fires.
    let activity = TurnActivity {
        proposed: 2,
        completed: 0,
        failed: 0,
        denied: 2,
        tools: BTreeMap::new(),
        denied_tools: BTreeMap::from([("Read".to_string(), 2)]),
    };
    assert!(activity.looks_ungrounded());
}

#[test]
fn turn_activity_grounding_gate_quiet_when_no_tools_proposed() {
    // Pure prose reply with no tool calls is valid grounding (the role
    // already had context from priors); never gate it.
    let activity = TurnActivity::default();
    assert!(!activity.looks_ungrounded());
}

#[test]
fn turn_activity_grounding_gate_quiet_on_partial_success_with_denials() {
    // 1 tool succeeded, 2 denied — the role had at least some grounded
    // information. The auto-route may still be valuable; don't gate.
    let activity = TurnActivity {
        proposed: 3,
        completed: 1,
        failed: 0,
        denied: 2,
        tools: BTreeMap::new(),
        denied_tools: BTreeMap::from([("Bash".to_string(), 2)]),
    };
    assert!(!activity.looks_ungrounded());
}

#[test]
fn turn_activity_top_denied_tools_orders_by_frequency() {
    let activity = TurnActivity {
        proposed: 5,
        completed: 0,
        failed: 0,
        denied: 5,
        tools: BTreeMap::new(),
        denied_tools: BTreeMap::from([
            ("Read".to_string(), 3),
            ("Bash".to_string(), 1),
            ("Glob".to_string(), 1),
        ]),
    };
    let top = activity.top_denied_tools(2);
    assert_eq!(top, vec!["Read", "Bash"]); // alphabetical tiebreak
}

#[test]
fn turn_activity_permission_denied_event_folds_into_denied_count() {
    let event = CrepEvent::PermissionDenied {
        role: "host".into(),
        tool_name: "Read".into(),
        tool_input: serde_json::json!({"file_path": "src/main.rs"}),
        reason: "denied by CodeRoom".into(),
    };
    let folded = TurnActivity::from_foldable_event(&event, "host").expect("foldable");
    assert_eq!(folded.denied, 1);
    assert_eq!(folded.denied_tools.get("Read"), Some(&1));
}

#[test]
fn status_region_advances_through_all_frames() {
    // Non-TTY mode prevents writes, so we can drive `advance()` purely
    // for state-machine coverage without polluting test output.
    let mut s = StatusRegion {
        slots: vec![StatusSlot {
            role: "backend".into(),
            frame: 0,
        }],
        is_painted: false,
        is_tty: false,
    };
    for expected in 1..=SPINNER_FRAMES.len() {
        s.advance();
        assert_eq!(s.slots[0].frame, expected % SPINNER_FRAMES.len());
    }
}

#[test]
fn status_region_clear_is_idempotent_and_marks_unpainted() {
    let mut s = StatusRegion {
        slots: vec![StatusSlot {
            role: "backend".into(),
            frame: 0,
        }],
        is_painted: true,
        is_tty: false,
    };
    s.clear();
    assert!(!s.is_painted);
    // second clear is a no-op
    s.clear();
    assert!(!s.is_painted);
}

#[test]
fn status_region_skips_paint_when_non_tty() {
    let mut s = StatusRegion {
        slots: vec![StatusSlot {
            role: "backend".into(),
            frame: 0,
        }],
        is_painted: false,
        is_tty: false,
    };
    s.repaint();
    // Non-TTY: paint should NOT mark as painted (so clear() doesn't
    // emit escape sequences into a redirected log either).
    assert!(!s.is_painted);
}

#[test]
fn status_region_reports_paused_chat_stream() {
    let s = StatusRegion {
        slots: vec![StatusSlot {
            role: "security".into(),
            frame: 0,
        }],
        is_painted: false,
        is_tty: false,
    };
    let rendered = strip_ansi(&s.render_line_at_width(80));
    assert_eq!(
        rendered,
        "│ 1 role working · chat stream paused until they report · @security ⠋"
    );
}

#[test]
fn spinner_frames_are_all_single_glyphs() {
    // Visual stability: every frame should be exactly one display
    // column wide so the trailing `›` (or end-of-line) doesn't jump
    // between frames.
    for (i, f) in SPINNER_FRAMES.iter().enumerate() {
        assert_eq!(f.chars().count(), 1, "frame {i} ({f:?}) is not 1 char");
    }
}

#[test]
fn parse_preserves_internal_whitespace() {
    match parse_line("@be   hello   world  ") {
        Command::SendTo { role, text } => {
            assert_eq!(role, "be");
            assert_eq!(text, "hello   world");
        }
        other => panic!("expected SendTo, got {other:?}"),
    }
}

#[test]
fn summarize_tool_input_shows_bash_command() {
    let v = serde_json::json!({"command": "ls -la"});
    let s = summarize_tool_input(&v);
    assert!(s.contains("ls -la"));
}

#[test]
fn summarize_tool_input_shows_file_path() {
    let v = serde_json::json!({"file_path": "src/main.rs"});
    assert_eq!(summarize_tool_input(&v), "src/main.rs");
}

#[test]
fn summarize_tool_input_falls_back_to_keys() {
    let v = serde_json::json!({"foo": 1, "bar": 2});
    let s = summarize_tool_input(&v);
    // HashMap iteration order isn't stable; just check both keys appear.
    assert!(s.contains("foo"));
    assert!(s.contains("bar"));
}

#[test]
fn truncate_inline_truncates_long_strings() {
    let out = truncate_inline("0123456789abcdef", 8);
    assert_eq!(out.chars().count(), 8);
    assert!(out.ends_with('…'));
}

#[test]
fn truncate_inline_preserves_short_strings() {
    assert_eq!(truncate_inline("hi", 8), "hi");
}
