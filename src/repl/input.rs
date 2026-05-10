use std::io::Write as _;

use anyhow::{Context, Result};
use crossterm::cursor::{MoveDown, MoveToColumn, MoveUp};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::queue;
use crossterm::style::Stylize;
use crossterm::terminal::{self, Clear, ClearType};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::output;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InputLine {
    Line(String),
    Eof,
    Interrupted,
}

pub(super) async fn read_tty_line(roles: Vec<String>) -> Result<InputLine> {
    tokio::task::spawn_blocking(move || read_tty_line_blocking(&roles))
        .await
        .context("joining tty input reader")?
}

fn read_tty_line_blocking(roles: &[String]) -> Result<InputLine> {
    let _raw_mode = RawModeGuard::enter()?;
    let columns = terminal::size().map_or(80, |(cols, _)| usize::from(cols));
    let mut editor = LineEditor::new(columns, roles.to_vec());
    let mut stdout = std::io::stdout();
    writeln!(stdout)?;
    editor.redraw(&mut stdout)?;

    loop {
        let Event::Key(key) = event::read().context("reading terminal input")? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                editor.finish(&mut stdout)?;
                writeln!(stdout, "^C")?;
                stdout.flush()?;
                return Ok(InputLine::Interrupted);
            }
            KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) && editor.is_empty() =>
            {
                editor.finish(&mut stdout)?;
                return Ok(InputLine::Eof);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                editor.clear();
                editor.redraw(&mut stdout)?;
            }
            KeyCode::Tab => {
                // First Tab: open the suggestion cycle. Subsequent
                // Tabs cycle the visible ghost. The user accepts by
                // pressing space / enter, both handled below; pressing
                // any non-completion key invalidates the cycle.
                if editor.cycle_completion() {
                    editor.redraw(&mut stdout)?;
                }
            }
            KeyCode::Char(' ')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                // Pressing space at an `@<prefix>` accepts the active
                // suggestion (if any) and inserts the trailing space
                // for the user — same behaviour as VSCode/Sublime
                // accepting a completion with space.
                if editor.ghost_suffix().is_some() {
                    editor.accept_completion();
                } else {
                    editor.insert(' ');
                }
                editor.redraw(&mut stdout)?;
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                editor.insert(ch);
                editor.redraw(&mut stdout)?;
            }
            KeyCode::Backspace => {
                if editor.backspace() {
                    editor.redraw(&mut stdout)?;
                }
            }
            KeyCode::Delete => {
                if editor.delete() {
                    editor.redraw(&mut stdout)?;
                }
            }
            KeyCode::Left => {
                if editor.move_left() {
                    editor.redraw(&mut stdout)?;
                }
            }
            KeyCode::Right => {
                if editor.move_right() {
                    editor.redraw(&mut stdout)?;
                }
            }
            KeyCode::Home => {
                if editor.move_home() {
                    editor.redraw(&mut stdout)?;
                }
            }
            KeyCode::End => {
                if editor.move_end() {
                    editor.redraw(&mut stdout)?;
                }
            }
            KeyCode::Enter => {
                let line = editor.input();
                editor.finish(&mut stdout)?;
                writeln!(stdout)?;
                stdout.flush()?;
                return Ok(InputLine::Line(line));
            }
            _ => {}
        }
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode().context("enabling raw terminal input")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Debug)]
struct LineEditor {
    prompt: String,
    prompt_width: usize,
    buffer: Vec<char>,
    cursor: usize,
    painted_cursor_width: usize,
    /// Display width of the trailing ghost-text completion (e.g. the
    /// remainder of `@back` → `@backend` shown in muted text after the
    /// cursor). Tracked so a redraw can wipe the right number of cells.
    painted_ghost_width: usize,
    columns: usize,
    /// Sorted role names available for `@` autocomplete.
    roles: Vec<String>,
    /// Index into [`Self::matching_roles`] used to cycle suggestions
    /// when the user presses Tab repeatedly. Reset every time the
    /// prefix changes.
    completion_index: usize,
    /// Buffer prefix (lower-cased) that produced [`Self::completion_index`].
    /// `None` means there is no active completion context yet.
    completion_anchor: Option<String>,
}

impl LineEditor {
    fn new(columns: usize, mut roles: Vec<String>) -> Self {
        // Stable ordering so the cycle order doesn't depend on HashMap
        // iteration noise; case-insensitive so `@H` cycles through
        // `@host`/`@helper` predictably.
        roles.sort_by_key(|name| name.to_ascii_lowercase());
        Self {
            prompt: output::prompt_inline(),
            prompt_width: UnicodeWidthStr::width(output::prompt_plain()),
            buffer: Vec::new(),
            cursor: 0,
            painted_cursor_width: 0,
            painted_ghost_width: 0,
            columns: columns.max(1),
            roles,
            completion_index: 0,
            completion_anchor: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    fn input(&self) -> String {
        self.buffer.iter().collect()
    }

    fn insert(&mut self, ch: char) {
        self.buffer.insert(self.cursor, ch);
        self.cursor += 1;
        self.invalidate_completion();
    }

    fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        self.buffer.remove(self.cursor);
        self.invalidate_completion();
        true
    }

    fn delete(&mut self) -> bool {
        if self.cursor >= self.buffer.len() {
            return false;
        }
        self.buffer.remove(self.cursor);
        self.invalidate_completion();
        true
    }

    fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        self.invalidate_completion();
        true
    }

    fn move_right(&mut self) -> bool {
        if self.cursor >= self.buffer.len() {
            return false;
        }
        self.cursor += 1;
        self.invalidate_completion();
        true
    }

    fn move_home(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor = 0;
        self.invalidate_completion();
        true
    }

    fn move_end(&mut self) -> bool {
        if self.cursor == self.buffer.len() {
            return false;
        }
        self.cursor = self.buffer.len();
        self.invalidate_completion();
        true
    }

    fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.invalidate_completion();
    }

    fn invalidate_completion(&mut self) {
        self.completion_anchor = None;
        self.completion_index = 0;
    }

    /// If the cursor is sitting at the end of an `@<prefix>` token (with
    /// the `@` either at buffer start or after whitespace), return the
    /// `(prefix_start_index, prefix_text)` so callers can compute
    /// completions and replacements.
    fn at_token(&self) -> Option<(usize, String)> {
        if self.cursor != self.buffer.len() {
            return None;
        }
        let mut idx = self.cursor;
        // Walk back to the most recent `@`, stopping at whitespace.
        while idx > 0 {
            let ch = self.buffer[idx - 1];
            if ch == '@' {
                let before_ok = idx == 1 || self.buffer[idx - 2].is_whitespace();
                if !before_ok {
                    return None;
                }
                let prefix: String = self.buffer[idx..self.cursor].iter().collect();
                return Some((idx - 1, prefix));
            }
            if ch.is_whitespace() {
                return None;
            }
            idx -= 1;
        }
        None
    }

    /// All role names whose lower-cased form starts with `prefix`
    /// (case-insensitive). Returned in the editor's stable role order.
    fn matching_roles(&self, prefix: &str) -> Vec<&str> {
        let needle = prefix.to_ascii_lowercase();
        self.roles
            .iter()
            .filter(|name| name.to_ascii_lowercase().starts_with(&needle))
            .map(String::as_str)
            .collect()
    }

    /// Currently displayed ghost-text completion, if any. Returns the
    /// suffix that would be appended to the `@<prefix>` if the user
    /// pressed Tab (e.g. for buffer `cr › @ba` against role `backend`,
    /// returns `"ckend"`).
    fn ghost_suffix(&self) -> Option<String> {
        let (_, prefix) = self.at_token()?;
        let matches = self.matching_roles(&prefix);
        if matches.is_empty() {
            return None;
        }
        let pick = matches[self.completion_index % matches.len()];
        // Skip if the prefix is already the full match (avoid empty ghost).
        if pick.eq_ignore_ascii_case(&prefix) {
            return None;
        }
        Some(pick[prefix.len()..].to_owned())
    }

    /// Cycle to the next role match for the active prefix. Returns
    /// `true` if the ghost text changed, `false` if there is no token
    /// or no matches.
    ///
    /// First Tab on a fresh prefix advances index 0 → 1 so the user
    /// gets visible feedback. Subsequent Tabs continue advancing and
    /// wrap around. Typing more characters resets the cycle via
    /// [`Self::invalidate_completion`].
    fn cycle_completion(&mut self) -> bool {
        let Some((_, prefix)) = self.at_token() else {
            return false;
        };
        let match_count = self.matching_roles(&prefix).len();
        if match_count == 0 {
            return false;
        }
        let prefix_key = prefix.to_ascii_lowercase();
        if self.completion_anchor.as_deref() == Some(prefix_key.as_str()) {
            self.completion_index = (self.completion_index + 1) % match_count;
        } else {
            self.completion_anchor = Some(prefix_key);
            // Lock and advance in the same step so a single Tab moves
            // off the index-0 ghost the user already sees.
            self.completion_index = 1 % match_count;
        }
        true
    }

    /// Replace the active `@<prefix>` with `@<full> ` (note trailing
    /// space). Returns `true` if a completion was applied.
    fn accept_completion(&mut self) -> bool {
        let Some((token_start, prefix)) = self.at_token() else {
            return false;
        };
        let matches = self.matching_roles(&prefix);
        if matches.is_empty() {
            return false;
        }
        let pick = matches[self.completion_index % matches.len()].to_owned();
        // Replace the buffer range [token_start, cursor) with `@<pick> `.
        self.buffer.drain(token_start..self.cursor);
        let mut insert_at = token_start;
        for ch in std::iter::once('@')
            .chain(pick.chars())
            .chain(std::iter::once(' '))
        {
            self.buffer.insert(insert_at, ch);
            insert_at += 1;
        }
        self.cursor = insert_at;
        self.invalidate_completion();
        true
    }

    fn redraw(&mut self, stdout: &mut std::io::Stdout) -> Result<()> {
        self.move_from_width_to_prompt_start(stdout, self.painted_cursor_width)?;
        queue!(stdout, Clear(ClearType::FromCursorDown))?;
        write!(stdout, "{}{}", self.prompt, self.input())?;
        let ghost = self.ghost_suffix().unwrap_or_default();
        let ghost_width = UnicodeWidthStr::width(ghost.as_str());
        if !ghost.is_empty() {
            write!(stdout, "{}", ghost.as_str().with(output::DIM))?;
        }
        let cursor_width = self.cursor_width();
        self.painted_ghost_width = ghost_width;
        // Cursor lands at the end of the user-visible buffer (before
        // the ghost suffix). The ghost is painted after but not
        // selectable.
        self.move_from_line_end_to_width(stdout, cursor_width)?;
        self.painted_cursor_width = cursor_width;
        stdout.flush()?;
        Ok(())
    }

    fn finish(&mut self, stdout: &mut std::io::Stdout) -> Result<()> {
        self.move_from_width_to_prompt_start(stdout, self.painted_cursor_width)?;
        // Wipe any pending ghost text before laying down the final line
        // so a leftover suggestion isn't echoed back to the user's terminal.
        write!(stdout, "{}{}", self.prompt, self.input())?;
        queue!(stdout, Clear(ClearType::UntilNewLine))?;
        let total_width = self.total_width();
        self.move_from_line_end_to_width(stdout, total_width)?;
        self.painted_cursor_width = total_width;
        self.painted_ghost_width = 0;
        stdout.flush()?;
        Ok(())
    }

    fn move_from_width_to_prompt_start(
        &self,
        stdout: &mut std::io::Stdout,
        width: usize,
    ) -> Result<()> {
        let (row, _) = self.visual_position(width);
        queue!(stdout, MoveToColumn(0))?;
        if row > 0 {
            queue!(stdout, MoveUp(saturating_u16(row)))?;
        }
        Ok(())
    }

    fn move_from_line_end_to_width(
        &self,
        stdout: &mut std::io::Stdout,
        width: usize,
    ) -> Result<()> {
        let (end_row, _) = self.visual_position(self.total_width());
        let (target_row, target_col) = self.visual_position(width);
        queue!(stdout, MoveToColumn(0))?;
        if end_row > 0 {
            queue!(stdout, MoveUp(saturating_u16(end_row)))?;
        }
        if target_row > 0 {
            queue!(stdout, MoveDown(saturating_u16(target_row)))?;
        }
        queue!(stdout, MoveToColumn(saturating_u16(target_col)))?;
        Ok(())
    }

    fn cursor_width(&self) -> usize {
        self.prompt_width + self.buffer_width_until(self.cursor)
    }

    fn total_width(&self) -> usize {
        // Includes the ghost suffix so wrap-aware cursor positioning
        // accounts for the full painted line, not just the user's
        // committed text.
        self.prompt_width + self.buffer_width_until(self.buffer.len()) + self.painted_ghost_width
    }

    fn buffer_width_until(&self, end: usize) -> usize {
        self.buffer
            .iter()
            .take(end)
            .map(|ch| UnicodeWidthChar::width(*ch).unwrap_or(0))
            .sum()
    }

    fn visual_position(&self, width: usize) -> (usize, usize) {
        (width / self.columns, width % self.columns)
    }
}

fn saturating_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_counts_cjk_cells_for_backspace() {
        let mut editor = LineEditor::new(80, Vec::new());
        editor.insert('物');
        editor.insert('物');
        editor.insert('品');

        assert_eq!(editor.input(), "物物品");
        assert_eq!(editor.buffer_width_until(editor.cursor), 6);

        assert!(editor.backspace());
        assert_eq!(editor.input(), "物物");
        assert_eq!(editor.buffer_width_until(editor.cursor), 4);
    }

    #[test]
    fn editor_tracks_cursor_width_separately_from_buffer_end() {
        let mut editor = LineEditor::new(80, Vec::new());
        editor.insert('物');
        editor.insert('a');
        editor.insert('品');
        assert!(editor.move_left());

        assert_eq!(editor.input(), "物a品");
        assert_eq!(
            editor.cursor_width(),
            UnicodeWidthStr::width(output::prompt_plain()) + 3
        );
        assert_eq!(
            editor.total_width(),
            UnicodeWidthStr::width(output::prompt_plain()) + 5
        );
    }

    #[test]
    fn editor_maps_wrapped_columns_by_display_width() {
        let mut editor = LineEditor::new(10, Vec::new());
        editor.insert('a');
        editor.insert('物');

        assert_eq!(editor.visual_position(editor.cursor_width()), (1, 1));
        editor.insert('品');
        assert_eq!(editor.visual_position(editor.cursor_width()), (1, 3));
    }

    fn role_editor() -> LineEditor {
        LineEditor::new(80, vec!["host".into(), "backend".into(), "ci".into()])
    }

    #[test]
    fn ghost_completes_role_after_at_at_buffer_start() {
        let mut editor = role_editor();
        editor.insert('@');
        editor.insert('b');
        assert_eq!(editor.ghost_suffix().as_deref(), Some("ackend"));
    }

    #[test]
    fn ghost_completes_role_after_whitespace_token() {
        let mut editor = role_editor();
        for ch in "ping @ho".chars() {
            editor.insert(ch);
        }
        assert_eq!(editor.ghost_suffix().as_deref(), Some("st"));
    }

    #[test]
    fn ghost_skips_when_at_is_not_at_token_start() {
        let mut editor = role_editor();
        for ch in "user@h".chars() {
            editor.insert(ch);
        }
        assert!(editor.ghost_suffix().is_none());
    }

    #[test]
    fn ghost_disappears_when_prefix_already_full_role_name() {
        let mut editor = role_editor();
        for ch in "@host".chars() {
            editor.insert(ch);
        }
        assert!(editor.ghost_suffix().is_none());
    }

    #[test]
    fn cycle_completion_rotates_through_matches() {
        let mut editor = LineEditor::new(80, vec!["chao".into(), "ci".into(), "core".into()]);
        editor.insert('@');
        editor.insert('c');
        let first = editor.ghost_suffix().expect("first match");
        assert!(editor.cycle_completion());
        let second = editor.ghost_suffix().expect("second match");
        assert_ne!(first, second);
        assert!(editor.cycle_completion());
        let third = editor.ghost_suffix().expect("third match");
        assert_ne!(second, third);
        assert!(editor.cycle_completion()); // wraps back to first
        assert_eq!(editor.ghost_suffix(), Some(first));
    }

    #[test]
    fn accept_completion_replaces_prefix_with_full_role_and_trailing_space() {
        let mut editor = role_editor();
        for ch in "@ba".chars() {
            editor.insert(ch);
        }
        assert!(editor.accept_completion());
        assert_eq!(editor.input(), "@backend ");
        assert_eq!(editor.cursor, editor.buffer.len());
    }

    #[test]
    fn accept_completion_preserves_text_before_token() {
        let mut editor = role_editor();
        for ch in "ping @ci_status".chars() {
            editor.insert(ch);
        }
        // Cursor must be at the end of the token for completion to fire.
        // Move back so the token "@ci_status" → ghost still works only
        // when the cursor is at the very end. After insertion above
        // cursor IS at the end, but `ci_status` is no longer prefix-only.
        // Confirm no ghost and no spurious accept.
        assert!(editor.ghost_suffix().is_none());
        assert!(!editor.accept_completion());
        assert_eq!(editor.input(), "ping @ci_status");
    }
}
