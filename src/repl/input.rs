use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::{MoveDown, MoveToColumn, MoveUp};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::queue;
use crossterm::style::Stylize;
use crossterm::terminal::{self, Clear, ClearType};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::output;
use crate::permissions::BridgeRequestSink;

use super::command::{SlashCommand, SLASH_COMMANDS};
use super::permission_prompt;

/// How often the input loop checks `bridge_rx` for pending permission
/// requests. 100 ms is short enough that a hook subprocess opens a
/// prompt within one frame; long enough that the loop isn't a busy-wait.
const BRIDGE_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InputLine {
    Line(String),
    Eof,
    Interrupted,
}

/// Read one line from the user's TTY. Ownership of the bridge receiver
/// is moved into the blocking thread for the duration of the read so
/// permission prompts that fire while the user is at the prompt can be
/// surfaced inline; it's returned to the caller as part of the result.
pub(super) async fn read_tty_line(
    roles: Vec<String>,
    bridge_rx: Option<tokio::sync::mpsc::Receiver<BridgeRequestSink>>,
    host_role: String,
) -> Result<(
    InputLine,
    Option<tokio::sync::mpsc::Receiver<BridgeRequestSink>>,
)> {
    tokio::task::spawn_blocking(move || read_tty_line_blocking(&roles, bridge_rx, &host_role))
        .await
        .context("joining tty input reader")?
}

fn read_tty_line_blocking(
    roles: &[String],
    mut bridge_rx: Option<tokio::sync::mpsc::Receiver<BridgeRequestSink>>,
    host_role: &str,
) -> Result<(
    InputLine,
    Option<tokio::sync::mpsc::Receiver<BridgeRequestSink>>,
)> {
    let _raw_mode = RawModeGuard::enter()?;
    let columns = terminal::size().map_or(80, |(cols, _)| usize::from(cols));
    let mut editor = LineEditor::new(columns, roles.to_vec());
    let mut stdout = std::io::stdout();
    writeln!(stdout)?;
    editor.redraw(&mut stdout)?;

    loop {
        // Drain any pending permission prompts BEFORE the next event
        // poll. This is what makes hooks that fire while the user is
        // at the prompt actually surface, instead of queuing silently
        // until the next role turn enters drain_one_turn.
        if let Some(rx) = bridge_rx.as_mut() {
            while let Ok(sink) = rx.try_recv() {
                editor.suspend(&mut stdout)?;
                permission_prompt::handle_request_blocking(sink, host_role);
                editor.redraw(&mut stdout)?;
            }
        }

        // Poll instead of blocking-read so the bridge check above can
        // fire on the next iteration even when the user isn't typing.
        if !event::poll(BRIDGE_POLL_INTERVAL).context("polling terminal input")? {
            continue;
        }

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
                return Ok((InputLine::Interrupted, bridge_rx));
            }
            KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) && editor.is_empty() =>
            {
                editor.finish(&mut stdout)?;
                return Ok((InputLine::Eof, bridge_rx));
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                editor.clear();
                editor.redraw(&mut stdout)?;
            }
            KeyCode::Tab => {
                // First Tab: open the suggestion cycle. Subsequent
                // Tabs cycle the visible ghost. Right Arrow / Ctrl-F
                // accept the visible ghost; Enter accepts then submits.
                if editor.cycle_completion() {
                    editor.redraw(&mut stdout)?;
                }
            }
            KeyCode::Right | KeyCode::Char('f')
                if matches!(key.code, KeyCode::Right)
                    || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                // zsh / fish convention: Right Arrow / Ctrl-F accepts
                // the visible ghost text. If no ghost, falls through
                // to ordinary cursor-move.
                if editor.ghost_suffix().is_some() {
                    editor.accept_completion();
                    editor.redraw(&mut stdout)?;
                } else if editor.move_right() {
                    editor.redraw(&mut stdout)?;
                }
            }
            KeyCode::Esc => {
                // Esc clears the active completion cycle but does NOT
                // dismiss the editor. Lets users explicitly say "no, I
                // wanted to type @b literally."
                if editor.dismiss_completion() {
                    editor.redraw(&mut stdout)?;
                }
            }
            KeyCode::Char(' ')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                // Space inserts a literal space and dismisses any
                // active ghost. Universal "I did NOT want that
                // completion" signal — matches zsh autosuggest, fish,
                // and shell readline norms.
                editor.dismiss_completion();
                editor.insert(' ');
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
                // Accept any visible ghost first so `@b\n` becomes
                // `@backend\n` instead of dispatching the prefix.
                // codex CLI external review caught this — it's the
                // exact prefix-submission failure this PR was meant to
                // eliminate.
                if editor.ghost_suffix().is_some() {
                    editor.accept_completion();
                }
                let line = editor.input();
                editor.finish(&mut stdout)?;
                writeln!(stdout)?;
                stdout.flush()?;
                return Ok((InputLine::Line(line), bridge_rx));
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

    /// Explicitly dismiss the active completion (e.g. on Esc / Space).
    /// Returns `true` if the visible state changed so callers know to
    /// trigger a redraw.
    fn dismiss_completion(&mut self) -> bool {
        let was_active = self.completion_anchor.is_some() || self.painted_ghost_width > 0;
        self.invalidate_completion();
        was_active
    }

    /// Erase the editor's painted text from the terminal so a
    /// permission prompt can paint over it cleanly. The cursor is left
    /// at the prompt-start position; a follow-up `redraw` repaints.
    fn suspend(&mut self, stdout: &mut std::io::Stdout) -> Result<()> {
        self.move_from_width_to_prompt_start(stdout, self.painted_cursor_width)?;
        queue!(stdout, Clear(ClearType::FromCursorDown))?;
        self.painted_cursor_width = 0;
        self.painted_ghost_width = 0;
        stdout.flush()?;
        Ok(())
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

    /// If the buffer is at-cursor-end and looks like a slash command in
    /// the middle of being typed (`/<prefix>` with no whitespace yet),
    /// return the bare prefix (no leading `/`). Once the user types a
    /// space, slash-command completion is done — the existing
    /// [`Self::at_token`] takes over for `@role` arg completion.
    fn slash_token(&self) -> Option<String> {
        if self.cursor != self.buffer.len() {
            return None;
        }
        if self.buffer.first() != Some(&'/') {
            return None;
        }
        let after_slash: String = self.buffer.iter().skip(1).collect();
        if after_slash.chars().any(char::is_whitespace) {
            return None;
        }
        Some(after_slash)
    }

    /// All slash commands whose name starts with `prefix` (case-insensitive).
    /// Returned in [`SLASH_COMMANDS`] declaration order (alphabetical), so
    /// the Tab cycle is stable. Associated function — the source is a
    /// static table, no editor state is involved.
    fn matching_slash_commands(prefix: &str) -> Vec<&'static SlashCommand> {
        let needle = prefix.to_ascii_lowercase();
        SLASH_COMMANDS
            .iter()
            .filter(|cmd| cmd.name.to_ascii_lowercase().starts_with(&needle))
            .collect()
    }

    /// Currently displayed ghost-text completion, if any. Returns the
    /// suffix that would be appended to the active token if the user
    /// pressed Tab — e.g. buffer `cr › @ba` against role `backend`
    /// returns `"ckend"`; buffer `cr › /h` returns `"alt"` (halt is the
    /// first alphabetical match in `halt`/`help`/`host`).
    fn ghost_suffix(&self) -> Option<String> {
        if let Some((_, prefix)) = self.at_token() {
            let matches = self.matching_roles(&prefix);
            if matches.is_empty() {
                return None;
            }
            let pick = matches[self.completion_index % matches.len()];
            if pick.eq_ignore_ascii_case(&prefix) {
                return None;
            }
            return Some(pick[prefix.len()..].to_owned());
        }
        if let Some(prefix) = self.slash_token() {
            let matches = Self::matching_slash_commands(&prefix);
            if matches.is_empty() {
                return None;
            }
            let pick = matches[self.completion_index % matches.len()];
            if pick.name.eq_ignore_ascii_case(&prefix) {
                return None;
            }
            return Some(pick.name[prefix.len()..].to_owned());
        }
        None
    }

    /// Cycle to the next match for the active token (either `@role` or
    /// `/command`). Returns `true` if the ghost text changed, `false`
    /// when there is no token or no matches.
    ///
    /// First Tab on a fresh prefix advances index 0 → 1 so the user
    /// gets visible feedback. Subsequent Tabs continue advancing and
    /// wrap around. Typing more characters resets the cycle via
    /// [`Self::invalidate_completion`].
    fn cycle_completion(&mut self) -> bool {
        let (prefix_key, match_count) = if let Some((_, prefix)) = self.at_token() {
            // `@`-prefixed key so the at-anchor and slash-anchor never
            // collide if both could match the same prefix string.
            let key = format!("@{}", prefix.to_ascii_lowercase());
            (key, self.matching_roles(&prefix).len())
        } else if let Some(prefix) = self.slash_token() {
            let key = format!("/{}", prefix.to_ascii_lowercase());
            (key, Self::matching_slash_commands(&prefix).len())
        } else {
            return false;
        };
        if match_count == 0 {
            return false;
        }
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

    /// Replace the active token with its picked completion. For `@role`
    /// the inserted form is `@<role> ` (trailing space). For `/command`
    /// the inserted form is `/<command>` with a trailing space only
    /// when the command takes arguments (e.g. `/halt `), and no
    /// trailing space for arg-less commands (e.g. `/help`). Returns
    /// `true` when a completion was applied.
    fn accept_completion(&mut self) -> bool {
        if let Some((token_start, prefix)) = self.at_token() {
            let matches = self.matching_roles(&prefix);
            if matches.is_empty() {
                return false;
            }
            let pick = matches[self.completion_index % matches.len()].to_owned();
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
            return true;
        }
        if let Some(prefix) = self.slash_token() {
            let matches = Self::matching_slash_commands(&prefix);
            if matches.is_empty() {
                return false;
            }
            let pick = *matches[self.completion_index % matches.len()];
            // `slash_token` guarantees the buffer starts with `/` and
            // the cursor sits at buffer end, so the range to replace is
            // the entire current buffer.
            self.buffer.drain(0..self.cursor);
            let mut insert_at = 0usize;
            for ch in std::iter::once('/').chain(pick.name.chars()) {
                self.buffer.insert(insert_at, ch);
                insert_at += 1;
            }
            if pick.takes_args {
                self.buffer.insert(insert_at, ' ');
                insert_at += 1;
            }
            self.cursor = insert_at;
            self.invalidate_completion();
            return true;
        }
        false
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

    // ---- slash command completion ------------------------------------

    fn slash_editor() -> LineEditor {
        // Roles are present so we can verify the `@` and `/` token
        // sources don't collide on overlapping prefixes ("h").
        LineEditor::new(80, vec!["host".into(), "backend".into()])
    }

    #[test]
    fn slash_ghost_completes_first_alphabetical_match() {
        let mut editor = slash_editor();
        editor.insert('/');
        editor.insert('h');
        // SLASH_COMMANDS in alphabetical order: allow, deny, exit, halt,
        // help, host, journal, patch, refresh, stop, transcript, welcome.
        // For prefix "h" the first match is `halt`.
        assert_eq!(editor.ghost_suffix().as_deref(), Some("alt"));
    }

    #[test]
    fn slash_cycle_rotates_through_matches() {
        let mut editor = slash_editor();
        editor.insert('/');
        editor.insert('h');
        // halt → help → host → halt
        let first = editor.ghost_suffix().expect("halt suffix");
        assert!(editor.cycle_completion());
        let second = editor.ghost_suffix().expect("help suffix");
        assert!(editor.cycle_completion());
        let third = editor.ghost_suffix().expect("host suffix");
        assert!(editor.cycle_completion());
        let wrapped = editor.ghost_suffix().expect("wrap to halt");
        assert_ne!(first, second);
        assert_ne!(second, third);
        assert_eq!(first, wrapped);
    }

    #[test]
    fn slash_accept_adds_trailing_space_for_args_commands() {
        let mut editor = slash_editor();
        editor.insert('/');
        editor.insert('a');
        // First match alphabetically for `a` is /allow (takes_args = true).
        assert!(editor.accept_completion());
        assert_eq!(editor.input(), "/allow ");
        assert_eq!(editor.cursor, editor.buffer.len());
    }

    #[test]
    fn slash_accept_omits_space_for_halt_so_enter_halts_everything() {
        // `/halt` alone halts every running role; that is the common
        // path. Tab-accept must leave the buffer ready for an immediate
        // Enter, not pad a space the user has to backspace away.
        let mut editor = slash_editor();
        for ch in "/ha".chars() {
            editor.insert(ch);
        }
        assert!(editor.accept_completion());
        assert_eq!(editor.input(), "/halt");
    }

    #[test]
    fn bare_slash_cycles_through_all_commands() {
        // Mirror of bare-`@` behavior, which also cycles through every
        // role when the prefix is empty. Locking this with a test so a
        // future "show menu only after one hint char" change is an
        // explicit decision rather than an accidental UX flip.
        let mut editor = slash_editor();
        editor.insert('/');
        assert!(editor.ghost_suffix().is_some());
    }

    #[test]
    fn slash_accept_omits_trailing_space_for_arg_less_commands() {
        let mut editor = slash_editor();
        for ch in "/exi".chars() {
            editor.insert(ch);
        }
        // /exit takes no args; accept lands the cursor right after `t`.
        assert!(editor.accept_completion());
        assert_eq!(editor.input(), "/exit");
        assert_eq!(editor.cursor, editor.buffer.len());
    }

    #[test]
    fn slash_completion_dormant_once_user_typed_space() {
        let mut editor = slash_editor();
        for ch in "/refresh ".chars() {
            editor.insert(ch);
        }
        // Whitespace ends the slash-command token; no slash ghost.
        assert!(editor.slash_token().is_none());
        // The user can now switch to @ completion for the role arg.
        editor.insert('@');
        editor.insert('b');
        assert_eq!(editor.ghost_suffix().as_deref(), Some("ackend"));
    }

    #[test]
    fn slash_ghost_skips_full_match() {
        let mut editor = slash_editor();
        for ch in "/help".chars() {
            editor.insert(ch);
        }
        // Buffer is the full command name; no ghost shown so the user
        // can immediately hit Enter without an accidental cycle.
        assert!(editor.ghost_suffix().is_none());
    }

    #[test]
    fn slash_token_requires_buffer_start() {
        let mut editor = slash_editor();
        for ch in "ask /halt".chars() {
            editor.insert(ch);
        }
        // `/halt` is mid-buffer, not a slash command — leave it alone.
        assert!(editor.slash_token().is_none());
        assert!(editor.ghost_suffix().is_none());
    }

    #[test]
    fn slash_no_match_produces_no_ghost() {
        let mut editor = slash_editor();
        for ch in "/zzz".chars() {
            editor.insert(ch);
        }
        assert!(editor.ghost_suffix().is_none());
        assert!(!editor.accept_completion());
    }
}
