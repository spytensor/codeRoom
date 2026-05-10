use crossterm::style::{Color, Stylize};
use tracing::debug;

use crate::crep::CrepEvent;
use crate::output;

use super::text::truncate_inline;

/// One-quarter block — thin, single-column vertical bar painted in a
/// role's stable color and prefixed onto every event line so the user
/// can tell at a glance which role is speaking, even on dense streams
/// where many `@`-tokens are visible at once.
const GUTTER: &str = "▎";

/// Render `body` with the role-color gutter on every line. Empty
/// trailing lines (a final `\n` in the body) are preserved without an
/// extra gutter so we don't paint a stray bar in a blank row.
fn with_gutter(role_paint: Color, body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 8);
    let mut iter = body.split_inclusive('\n').peekable();
    while let Some(line) = iter.next() {
        let (line_text, newline) = if let Some(stripped) = line.strip_suffix('\n') {
            (stripped, "\n")
        } else {
            (line, "")
        };
        if line_text.is_empty() && iter.peek().is_none() {
            // Trailing empty line — keep the newline but skip the gutter.
            out.push_str(newline);
        } else {
            out.push_str(&format!("{} {line_text}{newline}", GUTTER.with(role_paint)));
        }
    }
    out
}

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
        CrepEvent::RoleSpoke {
            role,
            text,
            cost_usd,
            ..
        } => {
            let _ = cost_usd;
            let role_paint = output::role_color(role, host_role);
            // First line carries the role token; subsequent lines just
            // get the gutter so the role is visually attributed even
            // for multi-paragraph replies.
            let mut lines = text.split_inclusive('\n');
            let first = lines.next().unwrap_or("");
            let head = format!(
                "{} {} {}",
                GUTTER.with(role_paint),
                output::role_token(role, host_role),
                first.strip_suffix('\n').unwrap_or(first),
            );
            let head = if first.ends_with('\n') {
                format!("{head}\n")
            } else {
                head
            };
            let rest: String = lines.collect();
            if rest.is_empty() {
                head
            } else {
                format!("{head}{}", with_gutter(role_paint, &rest))
            }
        }
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
                output_summary.as_str().with(output::DIM)
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
        CrepEvent::RoleStopped { role, reason } => {
            let role_paint = output::role_color(role, host_role);
            format!(
                "{} {}",
                GUTTER.with(role_paint),
                format!("@{role} stopped: {reason:?}")
                    .with(output::DIM)
                    .italic()
            )
        }
    }
}

pub(super) fn summarize_tool_input(input: &serde_json::Value) -> String {
    // Best-effort one-liner: if there's a "command", show it; if there's
    // a "file_path", show it; otherwise dump the JSON keys.
    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
        return format!("`{}`", truncate_inline(cmd, 80));
    }
    if let Some(path) = input.get("file_path").and_then(|v| v.as_str()) {
        return path.to_owned();
    }
    if let Some(obj) = input.as_object() {
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        return format!("({})", keys.join(", "));
    }
    String::new()
}
