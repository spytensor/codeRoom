use crossterm::style::Stylize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::output;

const MIN_TEXT_WIDTH: usize = 8;

/// Persistent markdown-rendering state for a streaming turn. Live
/// `RoleOutputDelta` events arrive in chunks; without persistent
/// state every chunk would render as a fresh markdown document —
/// the role badge would reprint at the head of each chunk and code
/// blocks opened in one chunk would close (visually) at the chunk
/// boundary because the local `in_code` flag resets. Keeping these
/// two flags across calls is what makes streaming output read like
/// one continuous reply.
///
/// One state value is created per turn drain and mutated by each
/// streamed render. The non-streaming entry point
/// [`render_role_markdown`] uses a fresh state per call (the
/// historical behavior) since a final `RoleSpoke` is a single
/// complete document.
#[derive(Debug, Clone, Default)]
pub(super) struct StreamMarkdownState {
    /// `true` until at least one non-blank line has been emitted in
    /// this turn. Switches to `false` after the first emit, so the
    /// `▎ @role` prefix only renders once per turn instead of once
    /// per streaming chunk.
    pub(super) first_line: bool,
    /// `true` when an opening ` ``` ` fence was seen but no closing
    /// fence has arrived yet. Persisted across chunks so a code
    /// block that spans two streaming deltas keeps the Code line
    /// style on both halves.
    pub(super) in_code: bool,
}

impl StreamMarkdownState {
    /// State for the start of a turn — first line not yet emitted,
    /// no open code fence.
    pub(super) fn fresh() -> Self {
        Self {
            first_line: true,
            in_code: false,
        }
    }
}

pub(super) fn render_role_markdown(
    role: &str,
    host_role: &str,
    text: &str,
    width: usize,
) -> String {
    let mut state = StreamMarkdownState::fresh();
    render_role_markdown_with_state(role, host_role, text, width, &mut state)
}

/// Streaming-aware variant of [`render_role_markdown`]. `state` is
/// the persistent flag bundle for the current turn; it is mutated to
/// reflect the post-render state so the next chunk picks up where
/// this one left off.
pub(super) fn render_role_markdown_with_state(
    role: &str,
    host_role: &str,
    text: &str,
    width: usize,
    state: &mut StreamMarkdownState,
) -> String {
    let role_color = output::role_color(role, host_role);
    let first_prefix = format!(
        "{} {} ",
        super::render::GUTTER.with(role_color),
        output::role_token(role, host_role)
    );
    let rest_prefix = format!("{} ", super::render::GUTTER.with(role_color));
    let first_plain = format!("{} @{role} ", super::render::GUTTER);
    let rest_plain = format!("{} ", super::render::GUTTER);
    let mut renderer = Renderer {
        width,
        first_prefix,
        rest_prefix,
        first_prefix_width: UnicodeWidthStr::width(first_plain.as_str()),
        rest_prefix_width: UnicodeWidthStr::width(rest_plain.as_str()),
        first_line: state.first_line,
        lines: Vec::new(),
    };
    state.in_code = render_blocks(text, &mut renderer, state.in_code);
    state.first_line = renderer.first_line;
    renderer.lines.join("\n")
}

struct Renderer {
    width: usize,
    first_prefix: String,
    rest_prefix: String,
    first_prefix_width: usize,
    rest_prefix_width: usize,
    first_line: bool,
    lines: Vec<String>,
}

impl Renderer {
    fn available(&self) -> usize {
        let prefix = if self.first_line {
            self.first_prefix_width
        } else {
            self.rest_prefix_width
        };
        self.width.saturating_sub(prefix).max(MIN_TEXT_WIDTH)
    }

    fn push_blank(&mut self) {
        let prefix = if self.first_line {
            &self.first_prefix
        } else {
            &self.rest_prefix
        };
        self.lines.push(prefix.trim_end().to_owned());
        self.first_line = false;
    }

    fn push_horizontal_rule(&mut self) {
        let prefix = if self.first_line {
            &self.first_prefix
        } else {
            &self.rest_prefix
        };
        let prefix_width = if self.first_line {
            self.first_prefix_width
        } else {
            self.rest_prefix_width
        };
        let dash_count = self.width.saturating_sub(prefix_width).max(MIN_TEXT_WIDTH);
        let rule = "─".repeat(dash_count).with(output::RULE).to_string();
        self.lines.push(format!("{prefix}{rule}"));
        self.first_line = false;
    }

    fn push_wrapped(&mut self, text: &str, style: LineStyle, continuation_indent: &str) {
        let available = self.available();
        let wrapped = wrap_cells(text, available, continuation_indent);
        if wrapped.is_empty() {
            self.push_blank();
            return;
        }
        for line in wrapped {
            let prefix = if self.first_line {
                &self.first_prefix
            } else {
                &self.rest_prefix
            };
            self.lines
                .push(format!("{prefix}{}", apply_line_style(&line, style)));
            self.first_line = false;
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum LineStyle {
    Normal,
    Emphasis,
    Code,
}

fn apply_line_style(text: &str, style: LineStyle) -> String {
    match style {
        LineStyle::Normal => render_inline_bold(text),
        LineStyle::Emphasis => strip_bold_markers(text).with(output::EM).bold().to_string(),
        LineStyle::Code => text.with(output::DIM).to_string(),
    }
}

fn render_inline_bold(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    let mut bold = false;
    while let Some(idx) = rest.find("**") {
        let (head, tail) = rest.split_at(idx);
        if bold {
            out.push_str(&head.with(output::TEXT).bold().to_string());
        } else {
            out.push_str(&head.with(output::TEXT).to_string());
        }
        rest = &tail[2..];
        bold = !bold;
    }
    if bold {
        out.push_str(&rest.with(output::TEXT).bold().to_string());
    } else {
        out.push_str(&rest.with(output::TEXT).to_string());
    }
    out
}

fn strip_bold_markers(text: &str) -> String {
    text.replace("**", "")
}

/// CommonMark thematic break: a line containing at least three
/// `-`, `_`, or `*` (mixed runs not allowed), optionally separated
/// by spaces. Examples: `---`, `***`, `___`, `- - -`.
fn is_horizontal_rule(line: &str) -> bool {
    for marker in ['-', '_', '*'] {
        let mut count = 0usize;
        let mut ok = true;
        for c in line.chars() {
            if c == marker {
                count += 1;
            } else if c.is_whitespace() {
                continue;
            } else {
                ok = false;
                break;
            }
        }
        if ok && count >= 3 {
            return true;
        }
    }
    false
}

/// Render `text` through the wrap-aware renderer. Takes the initial
/// in-code state (so streaming callers can persist it across chunks)
/// and returns the post-render in-code state. Non-streaming callers
/// pass `false` and ignore the return.
fn render_blocks(text: &str, renderer: &mut Renderer, mut in_code: bool) -> bool {
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            renderer.push_wrapped(line, LineStyle::Code, "");
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            renderer.push_blank();
            continue;
        }

        if is_horizontal_rule(trimmed) {
            // CommonMark thematic break — render as a dim dash run
            // filling the available column budget, instead of
            // dropping the literal `---` into the chat. Acts as a
            // visual section divider inside a single role's reply.
            renderer.push_horizontal_rule();
            continue;
        }

        if let Some(heading) = heading(trimmed) {
            renderer.push_wrapped(&strip_bold_markers(heading), LineStyle::Emphasis, "");
            continue;
        }

        if let Some(item) = bullet(trimmed) {
            renderer.push_wrapped(&format!("• {item}"), LineStyle::Normal, "  ");
            continue;
        }

        renderer.push_wrapped(trimmed, LineStyle::Normal, "");
    }
    in_code
}

fn heading(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=3).contains(&hashes) && line.chars().nth(hashes) == Some(' ') {
        Some(line[hashes + 1..].trim())
    } else {
        None
    }
}

fn bullet(line: &str) -> Option<&str> {
    let marker = line.chars().next()?;
    if matches!(marker, '-' | '*' | '+') && line.chars().nth(1) == Some(' ') {
        Some(line[2..].trim())
    } else {
        None
    }
}

fn wrap_cells(text: &str, width: usize, continuation_indent: &str) -> Vec<String> {
    let width = width.max(MIN_TEXT_WIDTH);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split_whitespace() {
        let word_width = UnicodeWidthStr::width(word);
        if word_width > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current.push_str(continuation_indent);
                current_width = UnicodeWidthStr::width(continuation_indent);
            }
            for chunk in hard_wrap_word(word, width) {
                if current_width + UnicodeWidthStr::width(chunk.as_str()) > width {
                    lines.push(std::mem::take(&mut current));
                    current.push_str(continuation_indent);
                    current_width = UnicodeWidthStr::width(continuation_indent);
                }
                current.push_str(&chunk);
                current_width += UnicodeWidthStr::width(chunk.as_str());
                if current_width >= width {
                    lines.push(std::mem::take(&mut current));
                    current.push_str(continuation_indent);
                    current_width = UnicodeWidthStr::width(continuation_indent);
                }
            }
            continue;
        }
        let separator = usize::from(!current.trim().is_empty());
        if current_width + separator + word_width > width {
            lines.push(std::mem::take(&mut current));
            current.push_str(continuation_indent);
            current_width = UnicodeWidthStr::width(continuation_indent);
        }
        if !current.trim().is_empty() {
            current.push(' ');
            current_width += 1;
        }
        current.push_str(word);
        current_width += word_width;
    }
    if !current.is_empty() || text.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_rule_recognized_in_canonical_shapes() {
        assert!(is_horizontal_rule("---"));
        assert!(is_horizontal_rule("***"));
        assert!(is_horizontal_rule("___"));
        // Mixed spacing is allowed per CommonMark.
        assert!(is_horizontal_rule("- - -"));
        assert!(is_horizontal_rule("  - - -  "));
        // Longer runs are still rules.
        assert!(is_horizontal_rule("------"));
    }

    #[test]
    fn horizontal_rule_rejected_for_non_rule_lines() {
        // Mixed markers don't count.
        assert!(!is_horizontal_rule("-*-"));
        // Fewer than three markers.
        assert!(!is_horizontal_rule("--"));
        assert!(!is_horizontal_rule("- -"));
        // Mixed with letters — keep as content.
        assert!(!is_horizontal_rule("--- end of section ---"));
        assert!(!is_horizontal_rule(""));
    }
}

fn hard_wrap_word(word: &str, width: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    for ch in word.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > width && !current.is_empty() {
            chunks.push(current);
            current = String::new();
            used = 0;
        }
        current.push(ch);
        used += ch_width;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}
