//! Render permission requests from the [`crate::permissions::bridge`]
//! and capture the user's single-keypress decision.
//!
//! The flow is intentionally synchronous from the user's perspective:
//! we paint a framed amber block describing what the engine wants to
//! run, switch the terminal into raw mode just long enough to read one
//! keystroke, then send the verdict back through the bridge responder.
//!
//! Visible UX:
//!
//! ```text
//!   ▎ @backend wants to run Bash  ─────────────────────────────────
//!   ▎ command: cargo test --no-run
//!   ▎ reason:  Bash requires approval under permission_mode=ask
//!   ▎ [a] allow once  [s] allow session  [d] deny once  [n] deny session
//! ```
//!
//! The `▎` left gutter is painted in the role's stable color so the
//! prompt is visually attributed even when many roles are active.

use std::io::Write as _;

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::Stylize;
use crossterm::terminal;

use crate::output;
use crate::permissions::{
    BridgeRequest, BridgeRequestSink, BridgeResponse, DecisionScope, PermissionDecision,
};

/// Largest input preview length, in displayable characters, before we
/// truncate with `…`. Chosen to fit comfortably on an 80-col terminal
/// after the gutter and label prefix.
const PREVIEW_MAX: usize = 64;

/// Paint the prompt for `request`, read one keypress, and send the
/// decision back through `request.responder`. Always sends a response —
/// either the parsed decision or a deny — even if rendering fails.
pub(super) async fn handle_request(sink: BridgeRequestSink, host_role: &str) -> Result<()> {
    let BridgeRequestSink { request, responder } = sink;
    paint_prompt(&request, host_role);

    let response = match read_decision_keypress().await {
        Ok(Some(response)) => response,
        Ok(None) => BridgeResponse::deny("declined: cancelled at prompt"),
        Err(error) => {
            output::bad(format!("permission prompt failed: {error:#}"));
            BridgeResponse::deny("CodeRoom prompt failed; defaulting to deny")
        }
    };
    paint_outcome(&request.role, host_role, &response);
    responder.respond(response);
    Ok(())
}

fn paint_prompt(request: &BridgeRequest, host_role: &str) {
    let role_paint = output::role_color(&request.role, host_role);
    let gutter = "▎".with(role_paint);
    let header = format!("@{} wants to run {}", request.role, request.tool)
        .with(role_paint)
        .bold();
    let rule_width = compute_rule_width();
    let rule = "─".repeat(rule_width).with(output::FADE);

    println!();
    println!("{gutter} {header}  {rule}");

    let summary = summarize_input(&request.input);
    if !summary.is_empty() {
        println!(
            "{gutter} {} {}",
            "input:".with(output::MUTE),
            summary.with(output::TEXT),
        );
    }

    let trimmed_reason = request.reason.trim();
    if !trimmed_reason.is_empty() {
        println!(
            "{gutter} {} {}",
            "reason:".with(output::MUTE),
            trimmed_reason.with(output::DIM),
        );
    }

    println!(
        "{gutter} {} {} {} {} {} {} {} {}",
        "[a]".with(output::OK).bold(),
        "allow once".with(output::TEXT),
        "[s]".with(output::OK).bold(),
        "allow session".with(output::TEXT),
        "[d]".with(output::BAD).bold(),
        "deny once".with(output::TEXT),
        "[n]".with(output::BAD).bold(),
        "deny session".with(output::TEXT),
    );
    print!("{gutter} ");
    let _ = std::io::stdout().flush();
}

fn paint_outcome(role: &str, host_role: &str, response: &BridgeResponse) {
    let role_paint = output::role_color(role, host_role);
    let gutter = "▎".with(role_paint);
    let (label, color) = match response.decision {
        PermissionDecision::Allow => ("allowed", output::OK),
        PermissionDecision::Deny => ("denied", output::BAD),
    };
    let scope_label = match response.scope {
        DecisionScope::Once => "once",
        DecisionScope::Session => "session",
    };
    println!(
        "\r\x1b[2K{gutter} {} {} {}",
        label.with(color).bold(),
        format!("({scope_label})").with(output::DIM),
        response.reason.as_str().with(output::DIM),
    );
    println!();
}

fn compute_rule_width() -> usize {
    let cols = crossterm::terminal::size().map_or(80, |(c, _)| usize::from(c));
    cols.saturating_sub(40).clamp(8, 60)
}

fn summarize_input(input: &serde_json::Value) -> String {
    if let Some(s) = input.get("command").and_then(|v| v.as_str()) {
        return truncate_preview(s);
    }
    if let Some(s) = input.get("file_path").and_then(|v| v.as_str()) {
        return truncate_preview(s);
    }
    if let Some(s) = input.get("path").and_then(|v| v.as_str()) {
        return truncate_preview(s);
    }
    if let Some(s) = input.get("pattern").and_then(|v| v.as_str()) {
        return truncate_preview(s);
    }
    if input.is_object() {
        // Fall back to a one-line JSON summary, capped.
        let s = input.to_string();
        return truncate_preview(&s);
    }
    String::new()
}

fn truncate_preview(s: &str) -> String {
    let collapsed = s.replace(['\n', '\r'], " ⏎ ");
    let count = collapsed.chars().count();
    if count <= PREVIEW_MAX {
        return collapsed;
    }
    let mut out: String = collapsed
        .chars()
        .take(PREVIEW_MAX.saturating_sub(1))
        .collect();
    out.push('…');
    out
}

/// Read one keypress in raw mode. Returns `Ok(Some(...))` for a known
/// answer, `Ok(None)` if the user pressed Esc / Ctrl-C (treated as
/// cancel → deny once), or `Err` if raw mode could not be entered.
async fn read_decision_keypress() -> Result<Option<BridgeResponse>> {
    tokio::task::spawn_blocking(read_decision_keypress_blocking)
        .await
        .context("joining permission keypress reader")?
}

fn read_decision_keypress_blocking() -> Result<Option<BridgeResponse>> {
    terminal::enable_raw_mode().context("entering raw mode for permission prompt")?;
    let result = (|| -> Result<Option<BridgeResponse>> {
        loop {
            match crossterm::event::read().context("reading permission keypress")? {
                Event::Key(key) => {
                    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                        continue;
                    }
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('c'))
                    {
                        return Ok(None);
                    }
                    match key.code {
                        KeyCode::Char('a' | 'A' | 'y' | 'Y') => {
                            return Ok(Some(decision(
                                PermissionDecision::Allow,
                                DecisionScope::Once,
                            )));
                        }
                        KeyCode::Char('s' | 'S') => {
                            return Ok(Some(decision(
                                PermissionDecision::Allow,
                                DecisionScope::Session,
                            )));
                        }
                        KeyCode::Char('d' | 'D') => {
                            return Ok(Some(decision(
                                PermissionDecision::Deny,
                                DecisionScope::Once,
                            )));
                        }
                        KeyCode::Char('n' | 'N') => {
                            return Ok(Some(decision(
                                PermissionDecision::Deny,
                                DecisionScope::Session,
                            )));
                        }
                        KeyCode::Esc => return Ok(None),
                        _ => continue,
                    }
                }
                _ => continue,
            }
        }
    })();
    let _ = terminal::disable_raw_mode();
    result
}

fn decision(decision: PermissionDecision, scope: DecisionScope) -> BridgeResponse {
    let reason = match (decision, scope) {
        (PermissionDecision::Allow, DecisionScope::Once) => "user allowed (once)",
        (PermissionDecision::Allow, DecisionScope::Session) => "user allowed (session)",
        (PermissionDecision::Deny, DecisionScope::Once) => "user denied (once)",
        (PermissionDecision::Deny, DecisionScope::Session) => "user denied (session)",
    };
    BridgeResponse {
        v: 1,
        decision,
        scope,
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truncate_preview_handles_long_strings() {
        let s = "a".repeat(100);
        let out = truncate_preview(&s);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= PREVIEW_MAX);
    }

    #[test]
    fn truncate_preview_collapses_newlines() {
        let s = "first line\nsecond line";
        let out = truncate_preview(s);
        assert!(out.contains('⏎'));
        assert!(!out.contains('\n'));
    }

    #[test]
    fn summarize_input_extracts_bash_command() {
        let input = json!({"command": "ls -la"});
        assert_eq!(summarize_input(&input), "ls -la");
    }

    #[test]
    fn summarize_input_extracts_file_path() {
        let input = json!({"file_path": "src/main.rs"});
        assert_eq!(summarize_input(&input), "src/main.rs");
    }

    #[test]
    fn summarize_input_falls_back_to_json_for_unknown_shape() {
        let input = json!({"weird_key": "weird_value"});
        let summary = summarize_input(&input);
        assert!(summary.contains("weird_key"));
    }
}
