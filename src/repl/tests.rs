use std::collections::BTreeMap;

use super::render::{render_event_line, render_event_line_at_width, summarize_tool_input};
use super::show::{filter_show_events, normalize_show_event};
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
            turn_id: String::new(),
            thread_id: String::new(),
        },
        CrepEvent::ToolCallProposed {
            role: "backend".into(),
            tool_name: "Read".into(),
            tool_input: serde_json::json!({"file_path": "README.md"}),
            tool_use_id: "tool-1".into(),
            turn_id: String::new(),
            thread_id: String::new(),
        },
        CrepEvent::ToolCallExecuted {
            role: "backend".into(),
            tool_use_id: "tool-1".into(),
            ok: true,
            output_summary: "README.md".into(),
            turn_id: String::new(),
            thread_id: String::new(),
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
        turn_id: None,
    }];
    let options = ShowOptions {
        role: None,
        since: None,
        tail: Some(0),
    };

    assert!(filter_show_events(&events, &options).is_empty());
}

#[test]
fn show_normalizes_legacy_cr_task_role_spoke() {
    let event = CrepEvent::RoleSpoke {
        role: "security".into(),
        text: "```cr-task\nReview permissions\n```\n\nFindings for @backend.".into(),
        mentions: vec!["backend".into()],
        cost_usd: 0.0,
        cache_read: 0,
        turn_id: String::new(),
        thread_id: String::new(),
    };

    let normalized = normalize_show_event(&event);

    assert_eq!(normalized.len(), 2);
    assert!(matches!(
        &normalized[0],
        CrepEvent::WorkTitle { role, title, .. }
            if role == "security" && title == "Review permissions"
    ));
    assert!(matches!(
        &normalized[1],
        CrepEvent::RoleSpoke { text, .. } if text == "Findings for @backend."
    ));
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
┌─ codeRoom v0.4.1 ────────────────────────────────────────────────────────────┐
│                                                                              │
│ welcome back, Ada              tips for getting started                      │
│                                • type @role to send a task to a specific ro… │
│ ● @backend   cc     · 1M       • /halt @role interrupts a turn; Ctrl-C twic… │
│ ● @host      cc     · 1M       • /journal <role> captures today's lessons-l… │
│ ● @security  codex  · default                                                │
│                                what's new in 0.4.1                           │
│  0  base tokens loaded         • role replies render as inset message blocks │
│ /repo/codeRoom                 • ready/work lifecycle chatter is hidden by … │
│                                • WorkCards sit inside the room instead of h… │
│                                                                              │
│                                /help for commands                            │
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
            turn_id: String::new(),
            thread_id: String::new(),
        },
        CrepEvent::RoleSpoke {
            role: "backend".into(),
            text: "Ready for @security.".into(),
            mentions: vec!["security".into()],
            cost_usd: 0.12,
            cache_read: 42,
            turn_id: String::new(),
            thread_id: String::new(),
        },
        CrepEvent::ToolCallProposed {
            role: "backend".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "cargo test --all-features"}),
            tool_use_id: "tool-1".into(),
            turn_id: String::new(),
            thread_id: String::new(),
        },
        CrepEvent::ToolCallExecuted {
            role: "backend".into(),
            tool_use_id: "tool-1".into(),
            ok: true,
            output_summary: "tests passed".into(),
            turn_id: String::new(),
            thread_id: String::new(),
        },
        CrepEvent::PermissionDenied {
            role: "backend".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "rm -rf target"}),
            reason: "destructive shell ops require review".into(),
            turn_id: String::new(),
            thread_id: String::new(),
        },
        CrepEvent::RoleStopped {
            role: "backend".into(),
            reason: StopReason::Refreshed,
            turn_id: None,
        },
    ];
    let rendered = events
        .iter()
        .map(|event| strip_ansi(&render_event_line(event, "host")))
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r"
  @backend
    Ready for @security.
    ↳ @backend · Bash `cargo test --all-features`
    ✓ @backend · tests passed
  ⊘ @backend · Bash denied: destructive shell ops require review
  @backend stopped: Refreshed
");
}

#[test]
fn multi_line_role_spoke_uses_inset_message_block() {
    let event = CrepEvent::RoleSpoke {
        role: "backend".into(),
        text: "First paragraph.\nSecond paragraph.\nThird paragraph.".into(),
        cost_usd: 0.0,
        cache_read: 0,
        mentions: vec![],
        turn_id: String::new(),
        thread_id: String::new(),
    };
    let rendered = strip_ansi(&render_event_line(&event, "host"));
    insta::assert_snapshot!(rendered, @r"
  @backend
    First paragraph.
    Second paragraph.
    Third paragraph.
");
}

#[test]
fn turn_dispatched_renders_as_full_width_handoff_banner() {
    // `starting` (queue_position == 0): the typical handoff. The
    // dash run fills the middle of the terminal so the speaker change
    // is unmistakable on a busy chat log. Width-property assertions
    // are more robust than character-exact snapshots when the dash
    // count depends on the role-name width.
    let dispatched = CrepEvent::TurnDispatched {
        role: "backend".into(),
        turn_id: String::new(),
        thread_id: String::new(),
        parent_turn_id: None,
        queue_position: 0,
    };
    let rendered = strip_ansi(&render_event_line_at_width(&dispatched, "host", 48));
    assert_eq!(unicode_width::UnicodeWidthStr::width(rendered.as_str()), 48);
    assert!(rendered.starts_with("  @backend "));
    assert!(rendered.ends_with(" starting"));
    assert!(rendered.contains('─'));

    // Queued behind two in-flight turns — kept as a terse italic
    // trace line rather than a banner so a long auto-route chain
    // doesn't paper the chat with section dividers for dispatches
    // that haven't actually changed the speaker on-screen.
    let queued = CrepEvent::TurnDispatched {
        role: "frontend".into(),
        turn_id: String::new(),
        thread_id: String::new(),
        parent_turn_id: None,
        queue_position: 2,
    };
    let rendered = strip_ansi(&render_event_line_at_width(&queued, "host", 60));
    assert_eq!(rendered, "  @frontend queued · 2 ahead");
}

#[test]
fn turn_dispatched_banner_pads_to_exact_width_in_tight_fits() {
    // Width budget exactly equal to fixed + 2 (1 dash position) used
    // to fall to the collapse branch and undershoot by 3 cells. Now
    // it pads with spaces so the rendered line is exactly `width`.
    let dispatched = CrepEvent::TurnDispatched {
        role: "backend".into(),
        turn_id: String::new(),
        thread_id: String::new(),
        parent_turn_id: None,
        queue_position: 0,
    };
    // fixed = 2 + |@backend|(8) + 2 + |starting|(8) = 20.
    // width = 22 → dash_count = 2 → space-padded branch.
    let rendered = strip_ansi(&render_event_line_at_width(&dispatched, "host", 22));
    assert_eq!(unicode_width::UnicodeWidthStr::width(rendered.as_str()), 22);
    assert!(rendered.starts_with("  @backend"));
    assert!(rendered.ends_with("starting"));
}

#[test]
fn turn_dispatched_banner_handles_long_role_names() {
    // Locks the width math against the role-name parameter — a long
    // role name eats more `fixed` budget so the dash run shrinks but
    // the total width remains exactly `width`.
    let dispatched = CrepEvent::TurnDispatched {
        role: "docs-reviewer".into(),
        turn_id: String::new(),
        thread_id: String::new(),
        parent_turn_id: None,
        queue_position: 0,
    };
    let rendered = strip_ansi(&render_event_line_at_width(&dispatched, "host", 60));
    assert_eq!(unicode_width::UnicodeWidthStr::width(rendered.as_str()), 60);
    assert!(rendered.starts_with("  @docs-reviewer "));
    assert!(rendered.ends_with(" starting"));
    assert!(rendered.contains('─'));
}

#[test]
fn format_reply_quote_shows_speaker_change_and_truncated_snippet() {
    use super::render::format_reply_quote;
    let parent_text = "Let me hand this off — @host should sanity-check the routing layer before we commit; I already verified the auth side.";
    let rendered = strip_ansi(&format_reply_quote(
        "host",
        "backend",
        "host",
        parent_text,
        72,
    ));
    assert!(!rendered.contains('\n'), "quote is intentionally one line");
    assert!(rendered.starts_with("  @host"));
    assert!(rendered.contains("↲"));
    assert!(rendered.contains("@backend"));
    assert!(rendered.ends_with('"'), "quote should be balanced");
}

#[test]
fn format_reply_quote_short_snippet_renders_without_ellipsis() {
    use super::render::format_reply_quote;
    let parent_text = "ok @host";
    let rendered = strip_ansi(&format_reply_quote(
        "host",
        "backend",
        "host",
        parent_text,
        80,
    ));
    assert!(!rendered.contains('\n'));
    assert!(rendered.contains("\"ok @host\""));
    assert!(!rendered.contains('…'));
}

#[test]
fn format_reply_quote_stays_one_line_at_narrow_width() {
    use super::render::format_reply_quote;
    let parent_text =
        "Look at src/server/index.ts and double-check the auth middleware before we ship.";
    let rendered = strip_ansi(&format_reply_quote(
        "host",
        "backend",
        "host",
        parent_text,
        40,
    ));
    assert!(!rendered.contains('\n'));
}

#[test]
fn format_reply_quote_collapses_whitespace_for_snippet() {
    use super::render::format_reply_quote;
    // Newlines and run-of-whitespace in the parent's text would
    // otherwise produce multi-line quotes that wreck the alignment.
    let parent_text = "First line.\n\nSecond paragraph\n  with    extra spaces.";
    let rendered = strip_ansi(&format_reply_quote(
        "host",
        "backend",
        "host",
        parent_text,
        80,
    ));
    assert!(!rendered.contains('\n'));
    assert!(rendered.contains("First line. Second paragraph with extra spaces."));
}

#[test]
fn turn_dispatched_collapses_on_narrow_terminals() {
    // On very narrow terminals the dash run shrinks to a single space
    // so the banner never wraps — the role badge + status still fit
    // on one line.
    let dispatched = CrepEvent::TurnDispatched {
        role: "backend".into(),
        turn_id: String::new(),
        thread_id: String::new(),
        parent_turn_id: None,
        queue_position: 0,
    };
    let rendered = strip_ansi(&render_event_line_at_width(&dispatched, "host", 16));
    // 16 cells: "  " (2) + "@backend" (8) + " " (1) + "starting" (8) = 19
    // → not enough for the dash run; fall back to a single space.
    assert_eq!(rendered, "  @backend starting");
}

#[test]
fn role_spoke_renders_markdown_lite_with_wrapping() {
    let event = CrepEvent::RoleSpoke {
        role: "security".into(),
        text: "# Main Risk\n\n- **Bash** can run broad commands that need review.\n\n```text\n# not a heading\n**not bold**\n```".into(),
        cost_usd: 0.0,
        cache_read: 0,
        mentions: vec![],
        turn_id: String::new(),
        thread_id: String::new(),
    };
    let rendered = strip_ansi(&render_event_line_at_width(&event, "host", 48));

    assert!(rendered.contains("Main Risk"));
    assert!(!rendered.contains("# Main Risk"));
    assert!(rendered.contains("• Bash can run broad"));
    assert!(!rendered.contains("**Bash**"));
    assert!(rendered.contains("# not a heading"));
    assert!(rendered.contains("**not bold**"));
    for line in rendered.lines() {
        assert!(
            unicode_width::UnicodeWidthStr::width(line) <= 48,
            "line too wide: {line:?}"
        );
    }
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
            turn_id: String::new(),
            thread_id: String::new(),
        },
        CrepEvent::ToolCallProposed {
            role: "host".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            tool_use_id: "2".into(),
            turn_id: String::new(),
            thread_id: String::new(),
        },
        CrepEvent::ToolCallExecuted {
            role: "host".into(),
            tool_use_id: "1".into(),
            ok: true,
            output_summary: "README.md".into(),
            turn_id: String::new(),
            thread_id: String::new(),
        },
        CrepEvent::ToolCallExecuted {
            role: "host".into(),
            tool_use_id: "2".into(),
            ok: true,
            output_summary: "Cargo.toml".into(),
            turn_id: String::new(),
            thread_id: String::new(),
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
        turn_id: String::new(),
        thread_id: String::new(),
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
        turn_id: String::new(),
        thread_id: String::new(),
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
        slots: vec![fixed_slot("backend")],
        is_painted: false,
        is_tty: false,
    };
    for expected in 1..=SPINNER_FRAMES.len() {
        s.advance();
        assert_eq!(s.slots[0].frame, expected % SPINNER_FRAMES.len());
    }
}

fn fixed_slot(role: &str) -> StatusSlot {
    // Pin started_at to "now" minus a known offset so the rendered
    // elapsed string is stable across test runs.
    StatusSlot {
        role: role.to_owned(),
        frame: 0,
        started_at: std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(12))
            .unwrap_or_else(std::time::Instant::now),
        tools_seen: 0,
        current_state: None,
    }
}

#[test]
fn status_region_clear_is_idempotent_and_marks_unpainted() {
    let mut s = StatusRegion {
        slots: vec![fixed_slot("backend")],
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
        slots: vec![fixed_slot("backend")],
        is_painted: false,
        is_tty: false,
    };
    s.repaint();
    // Non-TTY: paint should NOT mark as painted (so clear() doesn't
    // emit escape sequences into a redirected log either).
    assert!(!s.is_painted);
}

#[test]
fn status_region_renders_role_with_elapsed_and_state() {
    let s = StatusRegion {
        slots: vec![fixed_slot("security")],
        is_painted: false,
        is_tty: false,
    };
    let rendered = strip_ansi(&s.render_line_at_width(80));
    // 1 role · braille spinner frame · @security · elapsed (~12s,
    // give a tiny window for clock skew during the test) · default
    // "thinking" state since no events have arrived.
    assert!(
        rendered.starts_with("  1 role working · ⠋ @security · "),
        "unexpected status header: {rendered}"
    );
    assert!(rendered.contains("thinking"), "rendered: {rendered}");
}

#[test]
fn status_region_tracks_tool_count_and_state_from_events() {
    let mut s = StatusRegion {
        slots: vec![fixed_slot("security")],
        is_painted: false,
        is_tty: false,
    };
    s.update_from_event(&CrepEvent::ToolCallProposed {
        role: "security".into(),
        tool_name: "Bash".into(),
        tool_input: serde_json::json!({"command": "ls"}),
        tool_use_id: "t-1".into(),
        turn_id: String::new(),
        thread_id: String::new(),
    });
    s.update_from_event(&CrepEvent::ToolCallProposed {
        role: "security".into(),
        tool_name: "Read".into(),
        tool_input: serde_json::json!({"file_path": "Cargo.toml"}),
        tool_use_id: "t-2".into(),
        turn_id: String::new(),
        thread_id: String::new(),
    });
    let rendered = strip_ansi(&s.render_line_at_width(80));
    assert!(
        rendered.contains("· 2 tools · "),
        "expected tool count, got: {rendered}"
    );
    assert!(
        rendered.contains("running Read"),
        "latest tool should drive state: {rendered}"
    );
    assert!(
        rendered.contains("Cargo.toml"),
        "latest tool input should be visible: {rendered}"
    );
    // Events for a different role must not move the slot.
    s.update_from_event(&CrepEvent::ToolCallProposed {
        role: "other".into(),
        tool_name: "Edit".into(),
        tool_input: serde_json::json!({}),
        tool_use_id: "t-3".into(),
        turn_id: String::new(),
        thread_id: String::new(),
    });
    let rendered = strip_ansi(&s.render_line_at_width(80));
    assert!(rendered.contains("· 2 tools · "), "rendered: {rendered}");
}

#[test]
fn status_region_marks_and_clears_waiting_approval() {
    let mut s = StatusRegion {
        slots: vec![fixed_slot("security")],
        is_painted: false,
        is_tty: false,
    };
    s.mark_waiting_approval(
        "security",
        "Bash",
        &serde_json::json!({"command": "cargo test --workspace"}),
    );
    let rendered = strip_ansi(&s.render_line_at_width(120));
    assert!(rendered.contains("waiting approval · Bash `cargo test --workspace`"));

    s.clear_waiting_approval("security");
    let rendered = strip_ansi(&s.render_line_at_width(120));
    assert!(rendered.contains("thinking"));
    assert!(!rendered.contains("waiting approval"));
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
fn summarize_tool_input_collapses_multiline_commands() {
    let v = serde_json::json!({"command": "cd foo\ncargo test\t--locked"});
    let s = summarize_tool_input(&v);
    assert!(!s.contains('\n'), "status text must stay one-line: {s:?}");
    assert!(s.contains("cd foo cargo test --locked"), "got: {s}");
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

#[test]
fn parse_halt_no_arg() {
    assert_eq!(parse_line("/halt"), Command::Halt(None));
    assert_eq!(parse_line("/halt   "), Command::Halt(None));
}

#[test]
fn parse_halt_with_role() {
    assert_eq!(
        parse_line("/halt backend"),
        Command::Halt(Some("backend".into()))
    );
    // Tolerates `@` prefix and surrounding whitespace.
    assert_eq!(
        parse_line("/halt   @backend  "),
        Command::Halt(Some("backend".into()))
    );
    assert_eq!(parse_line("/halt @ci"), Command::Halt(Some("ci".into())));
}

#[test]
fn parse_halt_strips_at_only() {
    // Bare "@" is not a meaningful target; treat as "halt all".
    let parsed = parse_line("/halt @");
    assert!(matches!(parsed, Command::Halt(None)));
}

// ---- streaming-aware markdown ----------------------------------------
//
// `RoleOutputDelta` events arrive in chunks. The renderer used to be
// invoked with a fresh markdown context per chunk, which (a) reprinted
// the `@role` badge at the head of every chunk and (b) broke code
// blocks at chunk boundaries because the local `in_code` flag reset.
// The fix is a persistent `StreamMarkdownState` threaded across calls.

#[test]
fn streaming_state_emits_role_badge_only_on_first_chunk() {
    use crate::repl::markdown::{render_role_markdown_with_state, StreamMarkdownState};

    let mut state = StreamMarkdownState::fresh();
    let first = strip_ansi(&render_role_markdown_with_state(
        "security",
        "host",
        "First chunk text.",
        80,
        &mut state,
    ));
    let second = strip_ansi(&render_role_markdown_with_state(
        "security",
        "host",
        "Second chunk text.",
        80,
        &mut state,
    ));

    // First chunk carries the role header plus inset body.
    assert!(first.starts_with("  @security\n    "), "first: {first:?}");
    // Second chunk uses the body inset only.
    assert!(second.starts_with("    "), "second: {second:?}");
    assert!(
        !second.contains("@security"),
        "second chunk should not reprint the role badge: {second:?}"
    );
}

#[test]
fn streaming_state_persists_code_block_across_chunks() {
    use crate::repl::markdown::{render_role_markdown_with_state, StreamMarkdownState};

    // Opening fence in chunk 1; chunk 2 carries code body; closing
    // fence in chunk 3. All middle-chunk lines should render with the
    // Code style — i.e. dim, no `•`/heading processing applied — even
    // though chunk 2 standing alone has no `cr-task` opener.
    let mut state = StreamMarkdownState::fresh();
    let _ = render_role_markdown_with_state("security", "host", "```bash\n", 80, &mut state);
    assert!(state.in_code, "in_code should persist after opening fence");

    let mid = strip_ansi(&render_role_markdown_with_state(
        "security",
        "host",
        "npm audit --audit-level=high\n",
        80,
        &mut state,
    ));
    assert!(
        state.in_code,
        "in_code should still be set inside the block"
    );
    // Code text passes through verbatim — the bullet/heading detectors
    // shouldn't munge it, and there's no `•` injected.
    assert!(mid.contains("npm audit"), "mid: {mid:?}");
    assert!(
        !mid.contains("•"),
        "code body should not be bullet-rendered: {mid:?}"
    );

    let _ = render_role_markdown_with_state("security", "host", "```\n", 80, &mut state);
    assert!(!state.in_code, "closing fence flips in_code back off");
}

#[test]
fn streaming_state_resets_first_line_only_after_real_content() {
    use crate::repl::markdown::{render_role_markdown_with_state, StreamMarkdownState};

    // A pure-whitespace first chunk shouldn't burn the role header.
    // The header should still appear on the first chunk that actually
    // has visible text.
    let mut state = StreamMarkdownState::fresh();
    let _ = render_role_markdown_with_state("security", "host", "\n\n", 80, &mut state);
    assert!(
        state.first_line,
        "blank-only chunk should not consume the role header slot"
    );
}

// ---- filter_routable_mentions ----------------------------------------
//
// Tests for the worklist's mention-filtering helper. The dispatcher
// itself is async + cross-module and harder to drive in a unit test,
// but the routing decision is a pure function we can lock here.

#[test]
fn filter_routable_mentions_drops_self_mention() {
    use super::filter_routable_mentions;
    let known = &["host", "security", "backend"];
    let out = filter_routable_mentions(
        "security",
        &["security".to_owned(), "host".to_owned()],
        known,
    );
    assert_eq!(out, vec!["host".to_owned()]);
}

#[test]
fn filter_routable_mentions_drops_unknown_role() {
    use super::filter_routable_mentions;
    let known = &["host", "security"];
    let out = filter_routable_mentions(
        "host",
        &[
            "security".to_owned(),
            "nobody".to_owned(),
            "backend".to_owned(),
        ],
        known,
    );
    // `nobody` and `backend` aren't running; only `security` survives.
    assert_eq!(out, vec!["security".to_owned()]);
}

#[test]
fn filter_routable_mentions_preserves_order_and_duplicates() {
    // BFS dispatch order is preserved, including duplicates within a
    // single reply. A role mentioning `@peer` twice is two distinct
    // asks that may carry different conversational weight; dedup is
    // explicitly NOT applied at this layer.
    use super::filter_routable_mentions;
    let known = &["host", "security", "backend"];
    let out = filter_routable_mentions(
        "host",
        &[
            "backend".to_owned(),
            "security".to_owned(),
            "backend".to_owned(),
        ],
        known,
    );
    assert_eq!(
        out,
        vec![
            "backend".to_owned(),
            "security".to_owned(),
            "backend".to_owned(),
        ]
    );
}

#[test]
fn filter_routable_mentions_handles_empty_inputs() {
    use super::filter_routable_mentions;
    assert!(filter_routable_mentions("host", &[], &["host", "security"]).is_empty());
    // No known roles → every mention is "unknown", nothing routable.
    assert!(filter_routable_mentions("host", &["security".to_owned()], &[]).is_empty());
}

#[test]
fn turn_interrupted_finalizes_work_card() {
    // `drain_one_turn`'s new boundary handler should flip the
    // WorkCard to Interrupted on `CrepEvent::TurnInterrupted` for
    // the active role. Lock the WorkCard state machine here even
    // though the drain task itself is integration-tested only in
    // PR c's parallel rendering.
    use crate::repl::work::TurnWork;
    let mut work = TurnWork::new("security", "host", "scan repo");
    // Pre-existing tool step so the interrupted card has content.
    work.apply_event(&CrepEvent::ToolCallProposed {
        role: "security".into(),
        tool_name: "Bash".into(),
        tool_input: serde_json::json!({"command": "rg secret"}),
        tool_use_id: "tool-1".into(),
        turn_id: String::new(),
        thread_id: String::new(),
    });
    let card = work.interrupted_card("halted by user");
    assert_eq!(card.steps.len(), 1);
    match card.status {
        crate::output::work_card::WorkStatus::Interrupted { reason, .. } => {
            assert_eq!(reason, "halted by user");
        }
        other => panic!("expected Interrupted, got {other:?}"),
    }
}
