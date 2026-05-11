use crossterm::style::{Color, Stylize};
use crossterm::terminal;
use tracing::debug;
use unicode_width::UnicodeWidthStr;

use crate::crep::CrepEvent;
use crate::output;

use super::text::{one_line, truncate_inline};

/// One-quarter block — thin, single-column vertical bar painted in a
/// role's stable color and prefixed onto every event line so the user
/// can tell at a glance which role is speaking, even on dense streams
/// where many `@`-tokens are visible at once.
pub(super) const GUTTER: &str = "▎";

/// Trace-style gutter: same color as the role's main gutter but dimmer
/// so tool-call lines visually nest under their role's spoke without
/// drowning it out.
fn trace_gutter(role_paint: Color) -> String {
    GUTTER.with(role_paint).to_string()
}

fn is_placeholder_model(model: &str) -> bool {
    let normalized = model.trim().to_ascii_lowercase();
    normalized.is_empty() || normalized == "model"
}

pub(super) fn started_model_label(engine: &str, model: &str) -> String {
    if !is_placeholder_model(model) {
        return model.to_owned();
    }
    match engine {
        "cc" => "Claude default".to_owned(),
        "codex" => "Codex default".to_owned(),
        "gemini" => "Gemini default".to_owned(),
        other => format!("{other} default"),
    }
}

pub(super) fn render_event(event: &CrepEvent, host_role: &str) {
    println!("{}", render_event_line(event, host_role));
    if let CrepEvent::RoleSpoke { role, cost_usd, .. } = event {
        debug!(role, cost_usd, "RoleSpoke rendered");
    }
}

pub(super) fn render_event_line(event: &CrepEvent, host_role: &str) -> String {
    render_event_line_at_width(
        event,
        host_role,
        terminal::size().map_or(80, |(cols, _)| cols.into()),
    )
}

pub(super) fn render_event_line_at_width(
    event: &CrepEvent,
    host_role: &str,
    width: usize,
) -> String {
    match event {
        CrepEvent::RoleStarted {
            role,
            engine,
            model,
            ..
        } => {
            let model = started_model_label(engine, model);
            let role_paint = output::role_color(role, host_role);
            format!(
                "{} {}",
                GUTTER.with(role_paint),
                format!("@{role} ready · model={model}")
                    .with(output::DIM)
                    .italic()
            )
        }
        CrepEvent::WorkTitle { role, title, .. } => {
            let role_paint = output::role_color(role, host_role);
            format!(
                "{} {}",
                GUTTER.with(role_paint),
                format!("@{role} work · {title}").with(output::DIM).italic()
            )
        }
        CrepEvent::RoleSpoke {
            role,
            text,
            cost_usd,
            ..
        } => {
            let _ = cost_usd;
            super::markdown::render_role_markdown(role, host_role, text, width)
        }
        CrepEvent::RoleOutputDelta {
            role, text_delta, ..
        } => super::markdown::render_role_markdown(role, host_role, text_delta, width),
        CrepEvent::ToolCallProposed {
            role,
            tool_name,
            tool_input,
            ..
        } => {
            let summary = summarize_tool_input(tool_input);
            let role_paint = output::role_color(role, host_role);
            format!(
                "{} {} @{role} · {}",
                trace_gutter(role_paint),
                "↳".with(output::FADE),
                format!("{tool_name} {summary}").with(output::DIM),
            )
        }
        CrepEvent::ToolCallExecuted {
            role,
            ok,
            output_summary,
            ..
        } => {
            let role_paint = output::role_color(role, host_role);
            let glyph = if *ok {
                "✓".with(output::OK)
            } else {
                "✗".with(output::BAD)
            };
            format!(
                "{} {glyph} @{role} · {}",
                trace_gutter(role_paint),
                truncate_inline(&one_line(output_summary), 100).with(output::DIM)
            )
        }
        CrepEvent::PermissionDenied {
            role,
            tool_name,
            reason,
            ..
        } => {
            // Permission events get the warn color on the gutter too —
            // they're the one CREP variant where the role color is less
            // important than the security signal.
            format!(
                "{} {} @{role} · {}",
                GUTTER.with(output::WARN),
                "⊘".with(output::WARN),
                format!("{tool_name} denied: {reason}").with(output::DIM),
            )
        }
        CrepEvent::RoleStopped { role, reason, .. } => {
            let role_paint = output::role_color(role, host_role);
            format!(
                "{} {}",
                GUTTER.with(role_paint),
                format!("@{role} stopped: {reason:?}")
                    .with(output::DIM)
                    .italic()
            )
        }
        // `TurnDispatched` is the cross-role handoff boundary. When
        // the role is *actually starting* (queue_position == 0) we
        // render a full-width banner so the speaker change is an
        // unmistakable visual anchor. When the dispatch is queued
        // behind an in-flight turn we keep the old terse italic so
        // long chains of auto-routed dispatches don't paper the chat
        // with banners.
        CrepEvent::TurnDispatched {
            role,
            queue_position,
            ..
        } => {
            if *queue_position == 0 {
                handoff_banner(role, host_role, "starting", width)
            } else {
                let role_paint = output::role_color(role, host_role);
                format!(
                    "{} {}",
                    GUTTER.with(role_paint),
                    format!("@{role} queued · {queue_position} ahead")
                        .with(output::DIM)
                        .italic()
                )
            }
        }
        CrepEvent::TurnInterrupted { role, source, .. } => {
            let role_paint = output::role_color(role, host_role);
            format!(
                "{} {}",
                GUTTER.with(role_paint),
                format!("@{role} interrupted ({source:?})")
                    .with(output::DIM)
                    .italic()
            )
        }
    }
}

/// Build the role-handoff banner. Mirrors the layout of WorkCards
/// (gutter + role badge on the left, status on the right) but stretches
/// across the available terminal width so the speaker change reads as
/// a section divider, not a line of dim chatter.
///
/// Layout: `▎ @role ────────────────────────── status`. The dash run
/// shrinks gracefully as terminal width drops:
///
/// * `width >= fixed + 3` — dash separator fills the gap exactly so
///   the rendered line is exactly `width` cells.
/// * `width >= fixed`     — padded with spaces, line still exactly
///   `width` cells.
/// * `width < fixed`      — best-effort one space between role and
///   status; the line is `fixed - 1` cells. Realistic terminals are
///   ≥ 40 columns so this fallback rarely fires.
fn handoff_banner(role: &str, host_role: &str, status: &str, width: usize) -> String {
    let role_paint = output::role_color(role, host_role);
    let gutter = GUTTER.with(role_paint).to_string();
    let role_label = format!("@{role}").with(role_paint).bold().to_string();
    let role_plain = format!("@{role}");
    // 2 cells for "▎ ", role-label width, 1 separator space at each
    // end of the dashes, status width. Anything left over goes into
    // the dash run (or spaces, on a tight fit).
    let fixed =
        2 + UnicodeWidthStr::width(role_plain.as_str()) + 2 + UnicodeWidthStr::width(status);
    let dash_count = width.saturating_sub(fixed);
    let separator = if dash_count >= 3 {
        format!(" {} ", "─".repeat(dash_count))
            .with(output::RULE)
            .to_string()
    } else if width >= fixed {
        // 0 ≤ dash_count ≤ 2 — not enough dashes to read as a
        // divider, but we still pad with spaces so the total width
        // stays at `width` (status drifts left toward the role badge
        // rather than running off the right edge).
        " ".repeat(dash_count + 2)
    } else {
        // Terminal too narrow even for the role + status combo. Drop
        // the divider entirely; output will be `fixed - 1` cells and
        // will wrap if the terminal is genuinely smaller than that.
        " ".to_owned()
    };
    let status_styled = status.with(output::DIM).to_string();
    format!("{gutter} {role_label}{separator}{status_styled}")
}

/// Two-line quote block printed by the REPL just before dispatching
/// an auto-routed brief from `parent_role` to `child_role`. Parameter
/// order is `(child, parent)`: the child is the new speaker, the
/// parent is the role being quoted. Mirrors
/// the way Slack / Discord show a reply pointer:
///
/// ```text
/// ▎ @host → replying to @backend
/// ▎ │ "look at src/server, focus on the routing layer ..."
/// ```
///
/// The gutter color is the *child* role's color — the visual ownership
/// belongs to the new speaker because the block sits directly above
/// the child's turn output. `snippet` is one-line collapsed and
/// truncated so a long parent reply doesn't push the actual answer
/// off-screen.
pub(super) fn format_reply_quote(
    child_role: &str,
    parent_role: &str,
    host_role: &str,
    parent_text: &str,
    width: usize,
) -> String {
    let child_paint = output::role_color(child_role, host_role);
    let parent_paint = output::role_color(parent_role, host_role);
    // Gutter belongs to the child — the block sits directly above
    // the child's turn output so visual ownership is theirs.
    let gutter = GUTTER.with(child_paint).to_string();
    let child_label = format!("@{child_role}")
        .with(child_paint)
        .bold()
        .to_string();
    // The referenced parent name keeps its own role color so the
    // reader's eye links the quote back to that role's earlier output.
    let parent_label = format!("@{parent_role}").with(parent_paint).to_string();
    let arrow = "→".with(output::FADE).to_string();
    let reply_to = format!("replying to {parent_label}");
    let header = format!("{gutter} {child_label} {arrow} {reply_to}");

    // Reserve cells for `▎ │ "…"` plus a little breathing room.
    let snippet_budget = width.saturating_sub(8).max(20);
    let snippet_oneline = one_line(parent_text);
    let snippet = truncate_inline(&snippet_oneline, snippet_budget);
    let quote_text = format!("\"{snippet}\"").with(output::DIM).to_string();
    let quote = format!("{gutter} {sep} {quote_text}", sep = "│".with(output::RULE),);

    format!("{header}\n{quote}")
}

pub(super) fn summarize_tool_input(input: &serde_json::Value) -> String {
    // Best-effort one-liner: if there's a "command", show it; if there's
    // a "file_path", show it; otherwise dump the JSON keys.
    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
        return format!("`{}`", truncate_inline(&one_line(cmd), 80));
    }
    if let Some(path) = input.get("file_path").and_then(|v| v.as_str()) {
        return truncate_inline(&one_line(path), 80);
    }
    if let Some(obj) = input.as_object() {
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        return format!("({})", keys.join(", "));
    }
    String::new()
}
