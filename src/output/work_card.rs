//! Framed rendering for role work units.
//!
//! This module is intentionally pure: callers pass a structured
//! [`WorkCard`], a width, and get a string back. It does not parse engine
//! output, assign card ids, or infer protocol state.

use std::time::Duration;

use crossterm::style::{Color, Stylize};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{EM, FADE, OK, TEXT, WARN};

const MIN_WIDTH: usize = 28;
const MAX_WIDTH: usize = 120;
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A framed work card rendered in the terminal stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkCard {
    /// Configured role name, without a leading `@`.
    pub role: String,
    /// Stable display color for the role.
    pub role_color: Color,
    /// One-line task summary.
    pub title: String,
    /// Current card status.
    pub status: WorkStatus,
    /// Ordered work steps.
    pub steps: Vec<Step>,
    /// Whether a completed card should render in its compact form.
    pub collapsed: bool,
}

/// Runtime state represented by a [`WorkCard`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkStatus {
    /// The role is still working.
    Working {
        /// Spinner frame index.
        spinner_frame: usize,
        /// Current step summary, if known.
        current_step: Option<String>,
    },
    /// The role finished normally.
    Done {
        /// Elapsed time for the work unit.
        duration: Duration,
        /// Number of observed steps.
        steps_count: usize,
    },
    /// The role was interrupted or timed out.
    Interrupted {
        /// Human-readable interruption reason.
        reason: String,
    },
}

/// One line item inside a [`WorkCard`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Visual state of the step.
    pub kind: StepKind,
    /// One-line step text.
    pub text: String,
    /// Optional display timestamp or elapsed marker.
    pub time: Option<String>,
}

/// Step state used by card rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// Completed or terminal step.
    Done,
    /// Current active step.
    Active,
    /// Planned future step.
    Planned,
}

impl WorkCard {
    /// Render the card to a string with no trailing newline.
    #[must_use]
    pub fn render(&self, width: usize) -> String {
        let width = width.clamp(MIN_WIDTH, MAX_WIDTH);
        match &self.status {
            WorkStatus::Done { .. } if self.collapsed => self.render_done_collapsed(width),
            WorkStatus::Working { .. } => self.render_working(width),
            WorkStatus::Done { .. } => self.render_done_expanded(width),
            WorkStatus::Interrupted { .. } => self.render_interrupted(width),
        }
    }

    fn render_done_collapsed(&self, width: usize) -> String {
        let summary = match &self.status {
            WorkStatus::Done {
                duration,
                steps_count,
            } => format!(
                "@{} done · {} · {} · {} {}",
                self.role,
                self.title,
                format_duration(*duration),
                steps_count,
                if *steps_count == 1 { "step" } else { "steps" }
            ),
            _ => format!(
                "@{} · {} · {}",
                self.role,
                self.title,
                self.status_summary()
            ),
        };
        let content = truncate_cells(&summary, width.saturating_sub(2));
        format!("{} {}", "▎".with(self.border_color()), content.with(FADE))
    }

    fn render_working(&self, width: usize) -> String {
        let mut lines = vec![top_line(
            &format!("@{} · {}", self.role, self.title),
            width,
            self.border_color(),
        )];
        if let WorkStatus::Working {
            spinner_frame,
            current_step,
        } = &self.status
        {
            let frame = SPINNER_FRAMES[*spinner_frame % SPINNER_FRAMES.len()];
            let status = current_step
                .as_deref()
                .map_or_else(|| "working".to_owned(), |step| format!("working · {step}"));
            lines.push(content_line(
                &format!("{frame} {status}"),
                width,
                self.border_color(),
            ));
        }
        for step in self.steps.iter().take(4) {
            lines.push(content_line(&format_step(step), width, self.border_color()));
        }
        let hidden = self.steps.len().saturating_sub(4);
        if hidden > 0 {
            lines.push(content_line(
                &format!("· +{hidden} more in trace"),
                width,
                self.border_color(),
            ));
        }
        lines.push(bottom_line(width, self.border_color()));
        lines.join("\n")
    }

    fn render_done_expanded(&self, width: usize) -> String {
        let mut lines = vec![top_line(
            &format!("@{} · {}", self.role, self.title),
            width,
            self.border_color(),
        )];
        lines.push(content_line(
            &self.status_summary(),
            width,
            self.border_color(),
        ));
        for step in &self.steps {
            lines.push(content_line(&format_step(step), width, self.border_color()));
        }
        lines.push(bottom_line(width, self.border_color()));
        lines.join("\n")
    }

    fn render_interrupted(&self, width: usize) -> String {
        let mut lines = vec![top_line(
            &format!("@{} · {}", self.role, self.title),
            width,
            self.border_color(),
        )];
        lines.push(content_line(
            &self.status_summary(),
            width,
            self.border_color(),
        ));
        for step in self.steps.iter().take(4) {
            lines.push(content_line(&format_step(step), width, self.border_color()));
        }
        lines.push(bottom_line(width, self.border_color()));
        lines.join("\n")
    }

    fn status_summary(&self) -> String {
        match &self.status {
            WorkStatus::Working { current_step, .. } => current_step
                .as_deref()
                .map_or_else(|| "working".to_owned(), |step| format!("working · {step}")),
            WorkStatus::Done {
                duration,
                steps_count,
            } => format!(
                "done in {} · {} {}",
                format_duration(*duration),
                steps_count,
                if *steps_count == 1 { "step" } else { "steps" }
            ),
            WorkStatus::Interrupted { reason } => format!("interrupted · {reason}"),
        }
    }

    fn border_color(&self) -> Color {
        dim_role_color(self.role_color)
    }
}

fn top_line(label: &str, width: usize, color: Color) -> String {
    let inner_width = width.saturating_sub(2);
    let label = format!(
        "─ {} ",
        truncate_cells(label, inner_width.saturating_sub(2))
    );
    let fill = inner_width.saturating_sub(UnicodeWidthStr::width(label.as_str()));
    format!(
        "{}{}{}{}",
        "╭".with(color),
        label.with(color),
        "─".repeat(fill).with(color),
        "╮".with(color)
    )
}

fn bottom_line(width: usize, color: Color) -> String {
    let inner_width = width.saturating_sub(2);
    format!(
        "{}{}{}",
        "╰".with(color),
        "─".repeat(inner_width).with(color),
        "╯".with(color)
    )
}

fn content_line(text: &str, width: usize, color: Color) -> String {
    let inner_width = width.saturating_sub(4);
    let content = truncate_cells(text, inner_width);
    let padding = inner_width.saturating_sub(UnicodeWidthStr::width(content.as_str()));
    format!(
        "{} {}{} {}",
        "│".with(color),
        style_content(&content),
        " ".repeat(padding),
        "│".with(color)
    )
}

fn format_step(step: &Step) -> String {
    let glyph = match step.kind {
        StepKind::Done => "✓",
        StepKind::Active => "…",
        StepKind::Planned => "·",
    };
    match &step.time {
        Some(time) => format!("{glyph} [{time}] {}", step.text),
        None => format!("{glyph} {}", step.text),
    }
}

fn style_content(text: &str) -> String {
    // Step-shaped content: glyph + tool + rest. Three axes of color.
    if let Some(glyph_color) = step_glyph_color(text) {
        return style_step_line(text, glyph_color);
    }
    // Non-step content: status_summary's "interrupted · …" / "done
    // in …" / "working · …" headers. Single semantic color so the
    // header reads as one unit, not as a mis-parsed step.
    if text.starts_with("interrupted") {
        text.with(WARN).to_string()
    } else {
        text.with(TEXT).to_string()
    }
}

/// Tri-state glyph color for a step line. Returns `None` when the
/// text isn't shaped like a step (no leading `✓` / `…` / `·`).
fn step_glyph_color(text: &str) -> Option<Color> {
    if text.starts_with('✓') {
        Some(OK)
    } else if text.starts_with('…') {
        Some(EM)
    } else if text.starts_with('·') {
        Some(FADE)
    } else {
        None
    }
}

/// Style a step line of the shape `"<glyph> [<time>] <tool> <rest>"`.
/// Two-axis coloring: glyph_color (step state — done / active /
/// planned) and per-tool accent. Existing role-color gutter (in
/// `src/repl/render.rs`) is the third axis — orthogonal to both.
fn style_step_line(text: &str, glyph_color: Color) -> String {
    let mut chars = text.char_indices();
    let glyph_end = chars
        .by_ref()
        .find(|(_, c)| c.is_whitespace())
        .map_or(text.len(), |(i, _)| i);
    if glyph_end == text.len() {
        return text.with(glyph_color).to_string();
    }
    let glyph_part = &text[..glyph_end];
    let mut tail = &text[glyph_end..];
    let leading_ws = take_leading_ws(&mut tail);

    // Skip a leading `[time]` token if present so the tool-name
    // extraction lands on the actual tool, not the bracket.
    let time_part = take_bracket_token(&mut tail);
    let ws_after_time = if time_part.is_empty() {
        ""
    } else {
        take_leading_ws(&mut tail)
    };

    let (tool_name, rest) = match tail.find(char::is_whitespace) {
        Some(idx) => (&tail[..idx], &tail[idx..]),
        None => (tail, ""),
    };
    let accent = tool_accent_color(tool_name);

    if rest.is_empty() {
        format!(
            "{}{}{}{}{}",
            glyph_part.with(glyph_color),
            leading_ws,
            time_part.with(crate::output::DIM),
            ws_after_time,
            tool_name.with(accent),
        )
    } else {
        format!(
            "{}{}{}{}{}{}",
            glyph_part.with(glyph_color),
            leading_ws,
            time_part.with(crate::output::DIM),
            ws_after_time,
            tool_name.with(accent),
            rest.with(TEXT),
        )
    }
}

fn take_leading_ws<'a>(slice: &mut &'a str) -> &'a str {
    let trimmed = slice.trim_start();
    let ws_len = slice.len() - trimmed.len();
    let ws = &slice[..ws_len];
    *slice = trimmed;
    ws
}

/// Pull off a leading `[token]` if the slice starts with `[`. Returns
/// the empty string when there's no bracketed prefix. Used so a
/// `"[12s] Bash ls"` step renders the time in DIM and the tool name
/// in its proper accent, instead of the parser picking `"[12s]"` as
/// the tool name.
fn take_bracket_token<'a>(slice: &mut &'a str) -> &'a str {
    if !slice.starts_with('[') {
        return "";
    }
    if let Some(close) = slice.find(']') {
        let token = &slice[..=close];
        *slice = &slice[close + 1..];
        token
    } else {
        ""
    }
}

/// Per-tool accent used inside WorkCard steps. Reuses semantic
/// palette entries (no new colours): green for "executes", blue for
/// "reads", tan for "writes", amber for "delegates", muted-grey for
/// "searches" — five distinct hues that combine cleanly with the
/// per-role gutter colour without turning the card into a rainbow.
fn tool_accent_color(tool: &str) -> Color {
    use crate::output::{DIM, INFO, KEY, MUTE, SPLASH_ACCENT};
    match tool {
        "Bash" | "Run" | "Shell" => OK,
        "Read" | "Glob" | "LS" | "List" => INFO,
        "Edit" | "Write" | "MultiEdit" | "Append" => KEY,
        "Grep" | "Search" => MUTE,
        "Task" | "Agent" | "Subagent" | "delegate" => SPLASH_ACCENT,
        "denied" => WARN,
        _ => DIM,
    }
}

fn dim_role_color(color: Color) -> Color {
    match color {
        Color::Rgb { r, g, b } => Color::Rgb {
            r: dim_channel(r),
            g: dim_channel(g),
            b: dim_channel(b),
        },
        other => other,
    }
}

fn dim_channel(value: u8) -> u8 {
    let dimmed = ((u16::from(value) * 30) / 100).clamp(1, u16::from(u8::MAX));
    u8::try_from(dimmed).unwrap_or(u8::MAX)
}

fn truncate_cells(input: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(input) <= max_width {
        return input.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_owned();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in input.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width >= max_width {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    let secs = secs % 60;
    format!("{mins}m{secs:02}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for n in chars.by_ref() {
                    if n == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn sample_card() -> WorkCard {
        WorkCard {
            role: "security".into(),
            role_color: Color::Rgb {
                r: 0x5c,
                g: 0xd6,
                b: 0xcc,
            },
            title: "Scan repository permissions and adapters".into(),
            status: WorkStatus::Done {
                duration: Duration::from_secs(12),
                steps_count: 3,
            },
            collapsed: false,
            steps: vec![
                Step {
                    kind: StepKind::Done,
                    text: "Read Cargo.toml".into(),
                    time: None,
                },
                Step {
                    kind: StepKind::Done,
                    text: "Run cargo test".into(),
                    time: None,
                },
                Step {
                    kind: StepKind::Planned,
                    text: "Follow up on Claude subagent hooks".into(),
                    time: None,
                },
            ],
        }
    }

    #[test]
    fn done_collapsed_is_one_line_summary() {
        let mut card = sample_card();
        card.collapsed = true;
        let rendered = strip_ansi(&card.render(80));
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            "▎ @security done · Scan repository permissions and adapters · 12s · 3 steps"
        );
        assert!(!rendered.contains("Read Cargo.toml"));
    }

    #[test]
    fn expanded_card_renders_steps() {
        let rendered = strip_ansi(&sample_card().render(80));
        insta::assert_snapshot!(rendered, @r"
╭─ @security · Scan repository permissions and adapters ───────────────────────╮
│ done in 12s · 3 steps                                                        │
│ ✓ Read Cargo.toml                                                            │
│ ✓ Run cargo test                                                             │
│ · Follow up on Claude subagent hooks                                         │
╰──────────────────────────────────────────────────────────────────────────────╯
");
    }

    #[test]
    fn working_card_uses_bounded_height() {
        let card = WorkCard {
            status: WorkStatus::Working {
                spinner_frame: 2,
                current_step: Some("reading README.md".into()),
            },
            collapsed: false,
            ..sample_card()
        };
        let rendered = strip_ansi(&card.render(60));
        let count = rendered.lines().count();
        assert!((5..=8).contains(&count), "height was {count}");
        assert!(rendered.contains("working"));
    }

    #[test]
    fn long_cjk_title_stays_inside_width() {
        let mut card = sample_card();
        card.title = "全面扫描这个项目的安全风险并检查所有适配器权限边界".into();
        let rendered = strip_ansi(&card.render(40));
        for line in rendered.lines() {
            assert!(UnicodeWidthStr::width(line) <= 40, "{line:?} is too wide");
        }
    }

    #[test]
    fn tool_accent_color_covers_known_tools() {
        // Lock the accent palette so future refactors don't quietly
        // drift the per-tool color assignment that the WorkCard relies
        // on for at-a-glance task-type recognition.
        use crate::output::{INFO, KEY, MUTE, SPLASH_ACCENT};
        assert_eq!(tool_accent_color("Bash"), OK);
        assert_eq!(tool_accent_color("Run"), OK);
        assert_eq!(tool_accent_color("Shell"), OK);
        assert_eq!(tool_accent_color("Read"), INFO);
        assert_eq!(tool_accent_color("Glob"), INFO);
        assert_eq!(tool_accent_color("Edit"), KEY);
        assert_eq!(tool_accent_color("Write"), KEY);
        assert_eq!(tool_accent_color("MultiEdit"), KEY);
        assert_eq!(tool_accent_color("Grep"), MUTE);
        assert_eq!(tool_accent_color("Task"), SPLASH_ACCENT);
        assert_eq!(tool_accent_color("Agent"), SPLASH_ACCENT);
        assert_eq!(tool_accent_color("delegate"), SPLASH_ACCENT);
        assert_eq!(tool_accent_color("denied"), WARN);
        // Unknown tool falls to DIM — flag explicitly so a new
        // engine that emits e.g. "WebSearch" can pick up an accent
        // by adding a branch, not by accidentally inheriting DIM.
        assert_eq!(tool_accent_color("WebSearch"), crate::output::DIM);
    }

    #[test]
    fn style_content_interrupted_stays_single_color() {
        // Regression guard: the v0.2 PR c1 refactor of `style_content`
        // used to split the line into glyph + tool + rest, which
        // mis-colored the Interrupted summary "interrupted · halt by
        // user" into three different accents. The whole header stays
        // WARN.
        let styled = style_content("interrupted · halt by user");
        // Strip ANSI to see the bare text; we only care that no second
        // color escape sneaks in.
        let escape_count = styled.matches('\u{1b}').count();
        // One opening color + one reset = 2 escapes total.
        assert_eq!(
            escape_count, 2,
            "expected a single color span for the header, got: {styled:?}"
        );
    }

    #[test]
    fn style_content_step_with_time_prefix() {
        // `[12s]` is the optional Step.time prefix from format_step.
        // The parser must skip it so the tool-name accent lands on
        // the actual tool, not on the bracket.
        let styled = style_content("● [12s] Bash ls");
        // Three semantic regions: glyph (OK), [12s] (DIM), Bash (OK
        // again as Bash accent), ls (TEXT). The exact escape count
        // is brittle; just assert the bracket and the tool both
        // appear as bytes in the output and that the regions all
        // open separate color escapes.
        assert!(styled.contains("[12s]"));
        assert!(styled.contains("Bash"));
        assert!(styled.contains("ls"));
    }

    #[test]
    fn step_glyph_color_returns_none_for_non_step_text() {
        assert!(step_glyph_color("interrupted · halt").is_none());
        assert!(step_glyph_color("working · reading README.md").is_none());
        assert!(step_glyph_color("done in 12s · 3 steps").is_none());
        assert!(step_glyph_color("✓ Bash ls").is_some());
        assert!(step_glyph_color("… Read Cargo.toml").is_some());
        assert!(step_glyph_color("· planned").is_some());
    }
}
