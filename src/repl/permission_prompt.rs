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
//!   ▎ @backend wants Bash `cargo test --no-run` — [a]llow once · [s]ession · [d]eny · [n]ever
//! ```
//!
//! After a keypress the prompt row is overwritten in place with a
//! single-line outcome:
//!
//! ```text
//!   ▎ ✓ @backend allowed (session)
//! ```
//!
//! The `▎` left gutter is painted in the role's stable color so the
//! prompt is visually attributed even when many roles are active; the
//! `@role` label on the outcome keeps the attribution readable on
//! monochrome terminals and in `tee`-captured logs.
//!
//! Sessions in which the user chose "allow session" never re-prompt
//! for the same tool — `crate::permissions::decide_tool` short-circuits
//! via the persisted policy before the bridge fires.

use std::io::Write as _;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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

/// Poll interval for permission keypress reads. The async prompt path
/// runs the terminal read in `spawn_blocking`; polling instead of a
/// single indefinite `event::read()` lets cancellation drop raw mode
/// promptly when the surrounding role turn times out or is interrupted.
const KEYPRESS_POLL_INTERVAL: Duration = Duration::from_millis(100);

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

/// Synchronous variant of [`handle_request`] for callers already inside
/// a blocking thread (the TTY input loop). Skips spawn_blocking and
/// reads the keypress on the current thread. The terminal must already
/// be in raw mode — both the input loop's [`super::input::RawModeGuard`]
/// and this prompt's keypress reader share the same raw-mode session.
pub(super) fn handle_request_blocking(sink: BridgeRequestSink, host_role: &str) {
    let BridgeRequestSink { request, responder } = sink;
    paint_prompt(&request, host_role);
    let response = match read_decision_keypress_blocking_in_raw() {
        Ok(Some(response)) => response,
        Ok(None) => BridgeResponse::deny("declined: cancelled at prompt"),
        Err(error) => {
            output::bad(format!("permission prompt failed: {error:#}"));
            BridgeResponse::deny("CodeRoom prompt failed; defaulting to deny")
        }
    };
    paint_outcome(&request.role, host_role, &response);
    responder.respond(response);
}

/// Like [`read_decision_keypress_blocking`] but assumes the caller
/// already entered raw mode. Used from the TTY input loop where the
/// editor's RawModeGuard is still in scope.
fn read_decision_keypress_blocking_in_raw() -> Result<Option<BridgeResponse>> {
    read_decision_keypress_loop(|| true)
}

fn paint_prompt(request: &BridgeRequest, host_role: &str) {
    // print + flush, NOT println — the outcome line uses `\r\x1b[2K` to
    // overwrite this row in place. A trailing newline here would move
    // the cursor to the next row and the outcome would land *below*
    // the prompt instead of replacing it.
    let width = crossterm::terminal::size().map_or(80, |(c, _)| usize::from(c));
    print!("{}", format_prompt_line(request, host_role, width));
    let _ = std::io::stdout().flush();
}

/// Pure formatter for the one-line prompt — used by paint_prompt and
/// exposed for unit tests so the visible string is testable without a
/// terminal. The engine's "reason" string is intentionally dropped
/// from the visible line; for denied calls it is preserved on
/// `CrepEvent::PermissionDenied` for `cr show` replay, but for allowed
/// calls there is no equivalent event so the reason is not durably
/// recorded. The compact line keeps multiple in-session prompts from
/// stacking into a wall of approval ceremony.
///
/// `width` is the terminal column count and is used to keep the line
/// inside one row: when the choices ribbon plus the role/tool prefix
/// crowd the terminal, the tool-input summary is shortened so the
/// total stays under the budget.
fn format_prompt_line(request: &BridgeRequest, host_role: &str, width: usize) -> String {
    let role_paint = output::role_color(&request.role, host_role);
    let gutter = "▎".with(role_paint);
    let role_label = format!("@{}", request.role).with(role_paint).bold();
    let tool_label = request.tool.as_str().with(output::EM).bold();

    // Cells occupied by everything except the input summary. Mirrors
    // the format string below; kept as a const-ish list so the math
    // is checkable.
    let role_plain = format!("@{}", request.role);
    let prefix_cells = 2  // "▎ "
        + role_plain.chars().count()
        + " wants ".chars().count()
        + request.tool.chars().count()
        + " — ".chars().count();
    // Choices ribbon (`[a]llow once · [s]ession · [d]eny · [n]ever`)
    // in display cells, no styling.
    let choices_cells = "[a]llow once · [s]ession · [d]eny · [n]ever"
        .chars()
        .count();
    // Reserve 2 cells of breathing room so the line doesn't run flush
    // to the terminal edge. Floor the budget at 12 so very long Bash
    // commands still get a recognizable preview even on tight widths.
    let summary_budget = width
        .saturating_sub(prefix_cells + choices_cells + " ``".chars().count() + 2)
        .max(12);

    let raw_summary = summarize_input(&request.input);
    let summary_part = if raw_summary.is_empty() {
        String::new()
    } else {
        let bounded = truncate_to(&raw_summary, summary_budget);
        format!(" {}", format!("`{bounded}`").with(output::DIM))
    };
    // No space between `[a]` and `llow once`: the bracketed letter is
    // the visible mnemonic for the choice (`[a]` highlights the `a`
    // in `allow`), so they read as one word in the styled output.
    format!(
        "{gutter} {role_label} wants {tool_label}{summary_part} — {}{} · {}{} · {}{} · {}{}",
        "[a]".with(output::OK).bold(),
        "llow once".with(output::TEXT),
        "[s]".with(output::OK).bold(),
        "ession".with(output::TEXT),
        "[d]".with(output::BAD).bold(),
        "eny".with(output::TEXT),
        "[n]".with(output::BAD).bold(),
        "ever".with(output::TEXT),
    )
}

fn paint_outcome(role: &str, host_role: &str, response: &BridgeResponse) {
    // `\r\x1b[2K` returns to col 0 and clears the prompt line. We
    // don't print an outcome line for `allow once` — the subsequent
    // `✓ tool ok` trace already conveys success and stacking N
    // identical `▎ ✓ @role allowed (once)` lines per turn (one per
    // tool the user manually allowed) is pure noise. Denials and
    // session-scope answers DO emit because they're real state
    // transitions worth showing.
    let suppressed = matches!(
        (response.decision, response.scope),
        (PermissionDecision::Allow, DecisionScope::Once)
    );
    if suppressed {
        // Still wipe the prompt row so the next render starts on a
        // clean line.
        print!("\r\x1b[2K");
        let _ = std::io::stdout().flush();
        return;
    }
    println!(
        "\r\x1b[2K{}",
        format_outcome_line(role, host_role, response)
    );
}

/// Pure formatter for the one-line outcome echo — same testability
/// rationale as [`format_prompt_line`]. Even though the outcome
/// replaces the prompt in place, the role label is preserved so the
/// line is self-attributing in plain-text capture (`cr ... | tee`)
/// and on terminals without truecolor.
fn format_outcome_line(role: &str, host_role: &str, response: &BridgeResponse) -> String {
    let role_paint = output::role_color(role, host_role);
    let gutter = "▎".with(role_paint);
    let role_label = format!("@{role}").with(role_paint).bold();
    let (glyph, label, color) = match response.decision {
        PermissionDecision::Allow => ("✓", "allowed", output::OK),
        PermissionDecision::Deny => ("⊘", "denied", output::BAD),
    };
    let scope_label = match response.scope {
        DecisionScope::Once => "once",
        DecisionScope::Session => "session",
    };
    format!(
        "{gutter} {glyph_styled} {role_label} {action} ({scope_label})",
        glyph_styled = glyph.with(color).bold(),
        action = label.with(color),
    )
}

/// Char-count truncation with an ellipsis suffix — purpose-built for
/// the compact prompt line so it doesn't depend on the existing
/// `PREVIEW_MAX` constant which was tuned for the multi-line layout.
fn truncate_to(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
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
    let cancel = PromptReadCancel::new();
    let token = cancel.token();
    let result = tokio::task::spawn_blocking(move || read_decision_keypress_blocking(&token))
        .await
        .context("joining permission keypress reader")?;
    drop(cancel);
    result
}

#[derive(Debug)]
struct PromptReadCancel {
    keep_reading: Arc<AtomicBool>,
}

impl PromptReadCancel {
    fn new() -> Self {
        Self {
            keep_reading: Arc::new(AtomicBool::new(true)),
        }
    }

    fn token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.keep_reading)
    }
}

impl Drop for PromptReadCancel {
    fn drop(&mut self) {
        self.keep_reading.store(false, Ordering::Release);
    }
}

/// RAII guard for the brief raw-mode window the permission prompt opens
/// to read one keystroke. Drop runs on panic AND on early return so the
/// terminal is always restored — leaving raw mode on means the user's
/// shell stops echoing typed characters until they `stty sane`.
struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode().context("entering raw mode for permission prompt")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn read_decision_keypress_blocking(keep_reading: &AtomicBool) -> Result<Option<BridgeResponse>> {
    let _raw = RawModeGuard::enter()?;
    read_decision_keypress_loop(|| keep_reading.load(Ordering::Acquire))
}

fn read_decision_keypress_loop(
    mut keep_reading: impl FnMut() -> bool,
) -> Result<Option<BridgeResponse>> {
    while keep_reading() {
        if !crossterm::event::poll(KEYPRESS_POLL_INTERVAL).context("polling permission keypress")? {
            continue;
        }
        if let Event::Key(key) = crossterm::event::read().context("reading permission keypress")? {
            match decision_for_key(key) {
                PromptKeyDecision::Respond(response) => return Ok(Some(response)),
                PromptKeyDecision::Cancel => return Ok(None),
                PromptKeyDecision::Continue => {}
            }
        }
    }
    Ok(None)
}

#[derive(Debug, Clone)]
enum PromptKeyDecision {
    Respond(BridgeResponse),
    Cancel,
    Continue,
}

fn decision_for_key(key: KeyEvent) -> PromptKeyDecision {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return PromptKeyDecision::Continue;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        return PromptKeyDecision::Cancel;
    }
    match key.code {
        KeyCode::Char('a' | 'A' | 'y' | 'Y') => {
            PromptKeyDecision::Respond(decision(PermissionDecision::Allow, DecisionScope::Once))
        }
        KeyCode::Char('s' | 'S') => {
            PromptKeyDecision::Respond(decision(PermissionDecision::Allow, DecisionScope::Session))
        }
        KeyCode::Char('d' | 'D') => {
            PromptKeyDecision::Respond(decision(PermissionDecision::Deny, DecisionScope::Once))
        }
        KeyCode::Char('n' | 'N') => {
            PromptKeyDecision::Respond(decision(PermissionDecision::Deny, DecisionScope::Session))
        }
        KeyCode::Esc => PromptKeyDecision::Cancel,
        _ => PromptKeyDecision::Continue,
    }
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

    #[test]
    fn prompt_key_maps_allow_session() {
        let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        match decision_for_key(key) {
            PromptKeyDecision::Respond(response) => {
                assert_eq!(response.decision, PermissionDecision::Allow);
                assert_eq!(response.scope, DecisionScope::Session);
            }
            other => panic!("unexpected decision: {other:?}"),
        }
    }

    #[test]
    fn prompt_key_maps_ctrl_c_to_cancel() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(decision_for_key(key), PromptKeyDecision::Cancel));
    }

    #[test]
    fn prompt_key_ignores_unhandled_key() {
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(matches!(decision_for_key(key), PromptKeyDecision::Continue));
    }

    /// Strip ANSI escape sequences for substring-based assertions on
    /// styled lines.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for inner in chars.by_ref() {
                    if inner.is_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn bash_request(command: &str) -> BridgeRequest {
        BridgeRequest {
            v: 1,
            role: "backend".to_owned(),
            tool: "Bash".to_owned(),
            input: json!({"command": command}),
            reason: "Bash requires approval under permission_mode=ask".to_owned(),
        }
    }

    #[test]
    fn prompt_line_is_single_line_with_compact_choices() {
        let request = bash_request("cargo test --no-run");
        let line = strip_ansi(&format_prompt_line(&request, "host", 120));
        assert!(!line.contains('\n'), "prompt is a single line: {line:?}");
        assert!(line.starts_with("▎ @backend wants Bash"));
        assert!(line.contains("`cargo test --no-run`"));
        assert!(line.contains("[a]llow once"));
        assert!(line.contains("[s]ession"));
        assert!(line.contains("[d]eny"));
        assert!(line.contains("[n]ever"));
    }

    #[test]
    fn prompt_line_omits_summary_when_input_is_empty() {
        // Some tools (`/welcome`, `LS`) propose no useful input — the
        // backtick block should disappear, not show empty backticks.
        let request = BridgeRequest {
            v: 1,
            role: "backend".to_owned(),
            tool: "TodoRead".to_owned(),
            input: json!(null),
            reason: "approval required".to_owned(),
        };
        let line = strip_ansi(&format_prompt_line(&request, "host", 120));
        assert!(line.starts_with("▎ @backend wants TodoRead"));
        assert!(!line.contains("``"), "no empty backticks: {line:?}");
    }

    #[test]
    fn prompt_line_truncates_summary_on_narrow_terminals() {
        // On an 80-col terminal the choices ribbon plus the role/tool
        // prefix eats most of the budget, so a long command must
        // truncate with an ellipsis rather than overflow the row.
        let long_cmd = "cargo test --workspace --all-features --no-run -- --test-threads=4";
        let request = bash_request(long_cmd);
        let plain = strip_ansi(&format_prompt_line(&request, "host", 80));
        assert!(
            plain.contains('…'),
            "summary should be ellipsized: {plain:?}"
        );
        // The original command must NOT appear in full at 80 cols.
        assert!(
            !plain.contains(long_cmd),
            "long command should be truncated: {plain:?}"
        );
    }

    #[test]
    fn outcome_line_includes_role_label_and_glyph() {
        let response = BridgeResponse {
            v: 1,
            decision: PermissionDecision::Allow,
            scope: DecisionScope::Session,
            reason: "user allowed (session)".to_owned(),
        };
        let line = strip_ansi(&format_outcome_line("backend", "host", &response));
        assert_eq!(line, "▎ ✓ @backend allowed (session)");
    }

    #[test]
    fn outcome_line_denial_uses_deny_glyph() {
        let response = BridgeResponse {
            v: 1,
            decision: PermissionDecision::Deny,
            scope: DecisionScope::Once,
            reason: "user denied (once)".to_owned(),
        };
        let line = strip_ansi(&format_outcome_line("backend", "host", &response));
        assert_eq!(line, "▎ ⊘ @backend denied (once)");
    }

    #[test]
    fn prompt_read_cancel_drop_stops_reader_token() {
        let cancel = PromptReadCancel::new();
        let token = cancel.token();
        assert!(token.load(Ordering::Acquire));

        drop(cancel);
        assert!(!token.load(Ordering::Acquire));
    }
}
