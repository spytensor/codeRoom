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
        let label = format!(
            "@{} · {} · {}",
            self.role,
            self.title,
            self.status_summary()
        );
        [
            top_line(&label, width, self.border_color()),
            bottom_line(width, self.border_color()),
        ]
        .join("\n")
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
        for step in self.steps.iter().take(5) {
            lines.push(content_line(&format_step(step), width, self.border_color()));
        }
        while lines.len() < 4 {
            lines.push(content_line("", width, self.border_color()));
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
        StepKind::Done => "●",    // filled circle — "this is done"
        StepKind::Active => "○",  // open circle — "still working"
        StepKind::Planned => "·", // small dot — "queued, not started"
    };
    match &step.time {
        Some(time) => format!("{glyph} [{time}] {}", step.text),
        None => format!("{glyph} {}", step.text),
    }
}

fn style_content(text: &str) -> String {
    let glyph_color = if text.starts_with('●') {
        OK
    } else if text.starts_with('○') {
        EM
    } else if text.starts_with('·') {
        FADE
    } else if text.starts_with("interrupted") {
        WARN
    } else {
        TEXT
    };

    // Two-axis colour coding: the glyph carries the step state
    // (done/active/planned) in `glyph_color`; the first word after
    // the glyph is the tool name, which gets a per-tool accent so
    // a glance at the card sorts shell calls from file reads from
    // edits at a glance. Existing role-color gutter (in repl/render.rs)
    // is the third axis — orthogonal to both, so all three combine.
    let mut chars = text.char_indices();
    let glyph_end = chars
        .by_ref()
        .find(|(_, c)| c.is_whitespace())
        .map_or(text.len(), |(i, _)| i);
    if glyph_end == text.len() {
        return text.with(glyph_color).to_string();
    }
    let glyph_part = &text[..glyph_end];
    let tail = &text[glyph_end..];
    let tail_trim_start = tail.len() - tail.trim_start().len();
    let leading_ws = &tail[..tail_trim_start];
    let after_ws = &tail[tail_trim_start..];

    let (tool_name, rest) = match after_ws.find(char::is_whitespace) {
        Some(idx) => (&after_ws[..idx], &after_ws[idx..]),
        None => (after_ws, ""),
    };
    let accent = tool_accent_color(tool_name);
    if rest.is_empty() {
        format!(
            "{}{}{}",
            glyph_part.with(glyph_color),
            leading_ws,
            tool_name.with(accent),
        )
    } else {
        format!(
            "{}{}{}{}",
            glyph_part.with(glyph_color),
            leading_ws,
            tool_name.with(accent),
            rest.with(TEXT),
        )
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
    fn done_collapsed_is_two_lines() {
        let mut card = sample_card();
        card.collapsed = true;
        let rendered = strip_ansi(&card.render(80));
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("@security"));
        assert!(lines[0].contains("done in 12s"));
    }

    #[test]
    fn expanded_card_renders_steps() {
        let rendered = strip_ansi(&sample_card().render(80));
        insta::assert_snapshot!(rendered, @r"
╭─ @security · Scan repository permissions and adapters ───────────────────────╮
│ done in 12s · 3 steps                                                        │
│ ● Read Cargo.toml                                                            │
│ ● Run cargo test                                                             │
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
}
