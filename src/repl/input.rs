use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::{MoveDown, MoveToColumn, MoveUp};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::style::Stylize;
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::output;
use crate::permissions::BridgeRequestSink;

use super::command::{SlashCommand, SLASH_COMMANDS};
use super::permission_prompt;

/// How often the input loop checks `bridge_rx` for pending permission
/// requests. 100 ms is short enough that a hook subprocess opens a
/// prompt within one frame; long enough that the loop isn't a busy-wait.
const BRIDGE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum number of candidate rows the dropdown menu paints. Anything
/// beyond this prints a footer (`+N more (continue typing)`) so the
/// total visible height stays bounded — terminal heights as small as
/// 12 rows can still see the menu without scrolling the prompt off.
const MENU_MAX_VISIBLE: usize = 6;

/// Result of `LineEditor::accept_completion`: whether the completion
/// applied, and if so whether the resulting buffer expects more user
/// input before submission would be meaningful. Drives the Enter-key
/// behavior so a Tab-then-Enter on a role mention does not dispatch
/// an empty task (Slack/Discord convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptOutcome {
    /// No active completion or no matches — buffer unchanged.
    None,
    /// Completion applied and the resulting buffer is a self-contained
    /// command (e.g. `/help`); Enter should submit immediately.
    Complete,
    /// Completion applied but the resulting buffer expects the user
    /// to type an argument or task body (e.g. `@host `, `/halt `).
    /// Enter should accept the completion and wait for more input
    /// rather than dispatching an empty task.
    ExpectsMore,
}

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

        let event = event::read().context("reading terminal input")?;
        // Bracketed paste delivers the whole clipboard payload as one
        // event — including any embedded newlines. Without this branch
        // the terminal would synthesize per-line Enter key events and
        // each line would dispatch as its own prompt, which is what
        // made pasting a stack trace unusable before.
        if let Event::Paste(text) = event {
            editor.dismiss_completion();
            editor.insert_str(&text);
            editor.redraw(&mut stdout)?;
            continue;
        }
        let Event::Key(key) = event else {
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
            KeyCode::Tab | KeyCode::Down if editor.cycle_completion() => {
                // Tab and Down advance the selection in the menu /
                // ghost cycle. Down feels natural when the dropdown is
                // visible; Tab is the long-standing convention.
                editor.redraw(&mut stdout)?;
            }
            KeyCode::Up if editor.cycle_completion_back() => {
                // Up moves the menu selection backwards. Falls through
                // silently when there is no active completion.
                editor.redraw(&mut stdout)?;
            }
            KeyCode::Right | KeyCode::Char('f')
                if matches!(key.code, KeyCode::Right)
                    || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                // zsh / fish convention: Right Arrow / Ctrl-F accepts
                // the visible ghost text. If no ghost, falls through
                // to ordinary cursor-move.
                if editor.ghost_suffix().is_some() {
                    let _ = editor.accept_completion();
                    editor.redraw(&mut stdout)?;
                } else if editor.move_right() {
                    editor.redraw(&mut stdout)?;
                }
            }
            KeyCode::Esc if editor.dismiss_completion() => {
                // Esc clears the active completion cycle but does NOT
                // dismiss the editor. Lets users explicitly say "no, I
                // wanted to type @b literally."
                editor.redraw(&mut stdout)?;
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
            KeyCode::Backspace if editor.backspace() => {
                editor.redraw(&mut stdout)?;
            }
            KeyCode::Delete if editor.delete() => {
                editor.redraw(&mut stdout)?;
            }
            KeyCode::Left if editor.move_left() => {
                editor.redraw(&mut stdout)?;
            }
            KeyCode::Home if editor.move_home() => {
                editor.redraw(&mut stdout)?;
            }
            KeyCode::End if editor.move_end() => {
                editor.redraw(&mut stdout)?;
            }
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                // Shift+Enter (or Alt+Enter for terminals that swallow
                // SHIFT) inserts a literal newline so users can compose
                // multi-line prompts — stack traces, code blocks, SQL
                // — without each line dispatching as its own turn.
                // Mirrors Claude Code / Codex / Gemini conventions.
                editor.dismiss_completion();
                editor.insert('\n');
                editor.redraw(&mut stdout)?;
            }
            KeyCode::Enter => {
                // Accept any visible ghost first so `@b\n` becomes
                // `@backend\n` instead of dispatching the prefix.
                // codex CLI external review caught this — it's the
                // exact prefix-submission failure the original PR was
                // meant to eliminate.
                //
                // BUT: a fresh @-mention or `/<cmd-with-args>` accept
                // is "needs more input" — Enter should confirm the
                // completion and wait for the user to type the actual
                // task/argument, not dispatch an empty body to the
                // role. Arg-less slash commands (`/help`, `/exit`,
                // `/halt`, `/quit`, `/welcome`) keep submitting on
                // Enter because they are complete as typed.
                if editor.ghost_suffix().is_some() {
                    match editor.accept_completion() {
                        AcceptOutcome::ExpectsMore => {
                            editor.redraw(&mut stdout)?;
                            continue;
                        }
                        AcceptOutcome::Complete | AcceptOutcome::None => {}
                    }
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
        // Bracketed paste turns clipboard pastes into a single
        // `Event::Paste(String)` event instead of a flood of synthetic
        // key events that would dispatch each line as a separate prompt.
        // Failure here is non-fatal — fall back to per-key paste which
        // is the pre-PR behavior. Some minimal/legacy terminals (older
        // tmux, certain serial consoles) ignore the escape entirely.
        let _ = execute!(std::io::stdout(), EnableBracketedPaste);
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Debug)]
struct LineEditor {
    prompt: String,
    prompt_width: usize,
    buffer: Vec<char>,
    cursor: usize,
    /// Row offset (from the prompt's first painted row) where the
    /// cursor is currently sitting after the last `redraw`/`finish`.
    /// `clear_painted_area` uses this to walk back up to the prompt's
    /// first row before wiping. A buffer with embedded `\n` or
    /// soft-wrapped long lines spans multiple rows; this is how we
    /// keep track of where we are vertically.
    painted_cursor_row: usize,
    /// Display width of the trailing ghost-text completion (e.g. the
    /// remainder of `@back` → `@backend` shown in muted text after the
    /// cursor). Tracked so a redraw can wipe the right number of cells.
    painted_ghost_width: usize,
    columns: usize,
    /// Sorted role names available for `@` autocomplete.
    roles: Vec<String>,
    /// Index into the active candidate list (roles or slash commands)
    /// used to cycle suggestions when the user presses Tab, Up, or
    /// Down. Reset every time the prefix changes.
    completion_index: usize,
    /// Buffer prefix (namespaced as `@<prefix>` or `/<prefix>`) that
    /// produced [`Self::completion_index`]. `None` means there is no
    /// active completion context yet.
    completion_anchor: Option<String>,
    /// Number of dropdown-menu rows currently painted below the input.
    /// Tracked separately from `painted_ghost_width` because menu rows
    /// live in independent screen rows; redraw / suspend / finish all
    /// need to wipe these explicitly.
    painted_menu_rows: usize,
    /// `true` while the user has Esc-dismissed the menu for the current
    /// completion anchor. Cleared the next time the prefix changes so
    /// the dismissal is anchor-scoped, not editor-global.
    menu_dismissed: bool,
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
            painted_cursor_row: 0,
            painted_ghost_width: 0,
            columns: columns.max(1),
            roles,
            completion_index: 0,
            completion_anchor: None,
            painted_menu_rows: 0,
            menu_dismissed: false,
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

    /// Bulk-insert at the cursor. Normalises `\r\n`/`\r` to `\n` so a
    /// paste from Windows or older terminals lands as the same logical
    /// line breaks the buffer's render path expects. Discards any other
    /// control character (Tab is left intact — pasting Python source
    /// shouldn't lose its indentation).
    fn insert_str(&mut self, text: &str) {
        let chars: Vec<char> = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
            .collect();
        if chars.is_empty() {
            return;
        }
        let cursor = self.cursor;
        self.buffer.splice(cursor..cursor, chars.iter().copied());
        self.cursor = cursor + chars.len();
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
        // The dismissal is scoped to one completion anchor — once the
        // user changes the prefix they probably want fresh suggestions.
        self.menu_dismissed = false;
    }

    /// Explicitly dismiss the active completion (e.g. on Esc / Space).
    /// Resets the cycle and hides the dropdown menu for the current
    /// prefix; the ghost stays as a hint of the top match. The
    /// dismissal persists until the prefix changes (insert / backspace
    /// / delete), so a follow-up Tab does not surprise the user by
    /// re-opening the menu they just closed.
    ///
    /// Returns `true` when the visible state changed so callers know
    /// to trigger a redraw.
    fn dismiss_completion(&mut self) -> bool {
        // "Was anything to dismiss?" — checked from the conceptual
        // completion state so the answer is correct even before the
        // first redraw paints `painted_ghost_width` / `painted_menu_rows`.
        let was_active = self.completion_anchor.is_some() || self.ghost_suffix().is_some() || {
            let at = self
                .at_token()
                .is_some_and(|(_, prefix)| self.matching_roles(&prefix).len() >= 2);
            let slash = self
                .slash_token()
                .is_some_and(|prefix| Self::matching_slash_commands(&prefix).len() >= 2);
            (at || slash) && !self.menu_dismissed
        };
        self.completion_anchor = None;
        self.completion_index = 0;
        self.menu_dismissed = true;
        was_active
    }

    /// Erase the editor's painted text (including any dropdown menu
    /// below the input) from the terminal so a permission prompt can
    /// paint over it cleanly. The cursor is left at the prompt-start
    /// position; a follow-up `redraw` repaints.
    fn suspend(&mut self, stdout: &mut std::io::Stdout) -> Result<()> {
        self.clear_painted_area(stdout)?;
        stdout.flush()?;
        Ok(())
    }

    /// Move cursor to the prompt's first column on the first row and
    /// clear everything below. Used by [`Self::suspend`], at the start
    /// of [`Self::redraw`], and by [`Self::finish`] so the input area
    /// and any dropdown menu rows beneath it are wiped in one step.
    fn clear_painted_area(&mut self, stdout: &mut std::io::Stdout) -> Result<()> {
        queue!(stdout, MoveToColumn(0))?;
        if self.painted_cursor_row > 0 {
            queue!(stdout, MoveUp(saturating_u16(self.painted_cursor_row)))?;
        }
        queue!(stdout, Clear(ClearType::FromCursorDown))?;
        self.painted_cursor_row = 0;
        self.painted_ghost_width = 0;
        self.painted_menu_rows = 0;
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

    /// Rows to paint below the input as a dropdown menu. Empty when
    /// the active token (if any) has fewer than two candidates, when
    /// no token is active, or when the user has Esc-dismissed the menu
    /// for the current prefix.
    ///
    /// The selected row uses an emphasis color + `▎` leader. Up to
    /// [`MENU_MAX_VISIBLE`] rows render; an overflow footer of the form
    /// `+N more (continue typing)` is appended when the candidate
    /// list is longer.
    fn menu_rows(&self) -> Vec<String> {
        if self.menu_dismissed {
            return Vec::new();
        }
        // Cap visible rows at the terminal's height-budget so the
        // dropdown can never push the prompt off-screen on tiny
        // terminals. A budget below 2 disables the menu entirely —
        // the inline ghost is still shown.
        let budget = Self::menu_height_budget();
        if budget < 2 {
            return Vec::new();
        }
        let max_visible = MENU_MAX_VISIBLE.min(budget);
        if let Some((_, prefix)) = self.at_token() {
            let matches = self.matching_roles(&prefix);
            if matches.len() < 2 {
                return Vec::new();
            }
            let items: Vec<(String, String)> = matches
                .iter()
                .map(|name| ((*name).to_owned(), String::new()))
                .collect();
            return self.format_menu_rows(&items, max_visible);
        }
        if let Some(prefix) = self.slash_token() {
            let matches = Self::matching_slash_commands(&prefix);
            if matches.len() < 2 {
                return Vec::new();
            }
            let items: Vec<(String, String)> = matches
                .iter()
                .map(|cmd| (format!("/{}", cmd.name), cmd.description.to_owned()))
                .collect();
            return self.format_menu_rows(&items, max_visible);
        }
        Vec::new()
    }

    /// How many menu rows the terminal can show without scrolling the
    /// prompt off-screen. Reserves 2 rows for the input line + a bit
    /// of breathing room. Returns a generous default when terminal
    /// size can't be queried (non-TTY, in tests). Associated function —
    /// no editor state is involved.
    fn menu_height_budget() -> usize {
        let terminal_height = terminal::size().map_or(usize::MAX, |(_, rows)| usize::from(rows));
        terminal_height.saturating_sub(2)
    }

    /// Render the candidate list into styled terminal rows. Splits the
    /// name and description into two columns padded to the longest
    /// visible name. `max_visible` caps the rendered row count; any
    /// overflow becomes a `+N more (continue typing)` footer.
    fn format_menu_rows(&self, items: &[(String, String)], max_visible: usize) -> Vec<String> {
        let total = items.len();
        let visible = total.min(max_visible);
        let selected = self.completion_index % total;
        let name_width = items
            .iter()
            .take(visible)
            .map(|(name, _)| UnicodeWidthStr::width(name.as_str()))
            .max()
            .unwrap_or(0);

        let mut rows = Vec::with_capacity(visible + 1);
        for (idx, (name, description)) in items.iter().take(visible).enumerate() {
            let is_selected = idx == selected;
            let leader = if is_selected { "▎ " } else { "  " };
            let pad = name_width.saturating_sub(UnicodeWidthStr::width(name.as_str()));
            let body = if description.is_empty() {
                name.clone()
            } else {
                format!("{name}{}  {description}", " ".repeat(pad))
            };
            let styled = if is_selected {
                format!("{leader}{body}").with(output::EM).to_string()
            } else {
                format!("{leader}{body}").with(output::DIM).to_string()
            };
            rows.push(styled);
        }
        if total > visible {
            let footer = format!("  +{} more (continue typing)", total - visible);
            rows.push(footer.with(output::FADE).italic().to_string());
        }
        rows
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
        self.cycle_completion_step(1)
    }

    /// Cycle backwards through the candidate list — Up-arrow handler.
    fn cycle_completion_back(&mut self) -> bool {
        self.cycle_completion_step(-1)
    }

    /// Shared cycle implementation. `delta` is +1 for forward (Tab /
    /// Down) and -1 for backward (Up).
    fn cycle_completion_step(&mut self, delta: i32) -> bool {
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
        let count_i32 = i32::try_from(match_count).unwrap_or(i32::MAX);
        if self.completion_anchor.as_deref() == Some(prefix_key.as_str()) {
            let next =
                (i32::try_from(self.completion_index).unwrap_or(0) + delta).rem_euclid(count_i32);
            self.completion_index = usize::try_from(next).unwrap_or(0);
        } else {
            self.completion_anchor = Some(prefix_key);
            // Lock and advance in the same step so a single keypress
            // moves off the index-0 selection the user already sees.
            let start = delta.rem_euclid(count_i32);
            self.completion_index = usize::try_from(start).unwrap_or(0);
        }
        true
    }

    /// Replace the active token with its picked completion. For `@role`
    /// the inserted form is `@<role> ` (trailing space). For `/command`
    /// the inserted form is `/<command>` with a trailing space only
    /// when the command takes arguments (e.g. `/halt `), and no
    /// trailing space for arg-less commands (e.g. `/help`). Returns an
    /// [`AcceptOutcome`] so the Enter key handler can distinguish
    /// "complete command, submit now" from "needs more input, just
    /// confirm the completion".
    fn accept_completion(&mut self) -> AcceptOutcome {
        if let Some((token_start, prefix)) = self.at_token() {
            let matches = self.matching_roles(&prefix);
            if matches.is_empty() {
                return AcceptOutcome::None;
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
            // A bare @-mention is never a complete request — the
            // role still needs a task body.
            return AcceptOutcome::ExpectsMore;
        }
        if let Some(prefix) = self.slash_token() {
            let matches = Self::matching_slash_commands(&prefix);
            if matches.is_empty() {
                return AcceptOutcome::None;
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
            return if pick.takes_args {
                AcceptOutcome::ExpectsMore
            } else {
                AcceptOutcome::Complete
            };
        }
        AcceptOutcome::None
    }

    fn redraw(&mut self, stdout: &mut std::io::Stdout) -> Result<()> {
        self.clear_painted_area(stdout)?;
        write!(stdout, "{}", self.prompt)?;
        self.write_buffer(stdout)?;
        let ghost = self.ghost_suffix().unwrap_or_default();
        let ghost_width = UnicodeWidthStr::width(ghost.as_str());
        if !ghost.is_empty() {
            write!(stdout, "{}", ghost.as_str().with(output::DIM))?;
        }
        self.painted_ghost_width = ghost_width;

        // Paint the dropdown menu beneath the input. Each `\r\n` jumps
        // to column 0 of the next row, then we write the row content;
        // the cursor ends at the end of the final menu row.
        let menu_rows = self.menu_rows();
        let menu_height = menu_rows.len();
        for row in &menu_rows {
            write!(stdout, "\r\n{row}")?;
        }
        self.painted_menu_rows = menu_height;

        // Cursor lands inside the buffer at (cursor_row, cursor_col).
        // The ghost is painted past the buffer end but not selectable;
        // the menu lives in its own rows beneath everything.
        let (cursor_row, cursor_col) = self.cursor_position();
        self.move_from_painted_end_to(stdout, cursor_row, cursor_col)?;
        self.painted_cursor_row = cursor_row;
        stdout.flush()?;
        Ok(())
    }

    fn finish(&mut self, stdout: &mut std::io::Stdout) -> Result<()> {
        // Wipe the input area plus any dropdown menu before laying
        // down the final committed line so leftover suggestions aren't
        // echoed back to the user's terminal.
        self.clear_painted_area(stdout)?;
        write!(stdout, "{}", self.prompt)?;
        self.write_buffer(stdout)?;
        let (end_row, _) = self.position_at(self.buffer.len());
        self.painted_cursor_row = end_row;
        stdout.flush()?;
        Ok(())
    }

    /// Stream the buffer to `stdout`, translating embedded `\n` into
    /// `\r\n` so the terminal advances to column 0 of the next row.
    /// Tabs are kept; other control chars never enter the buffer
    /// (filtered at [`Self::insert_str`]) so we don't need to handle
    /// them here.
    fn write_buffer(&self, stdout: &mut std::io::Stdout) -> Result<()> {
        let mut chunk = String::new();
        for ch in &self.buffer {
            if *ch == '\n' {
                if !chunk.is_empty() {
                    write!(stdout, "{chunk}")?;
                    chunk.clear();
                }
                write!(stdout, "\r\n")?;
                continue;
            }
            chunk.push(*ch);
        }
        if !chunk.is_empty() {
            write!(stdout, "{chunk}")?;
        }
        Ok(())
    }

    /// Move the cursor from "end of the painted area" (last menu row
    /// when the menu is open; end of input + ghost otherwise) back
    /// into the input area at `(target_row, target_col)` relative to
    /// the prompt's first row.
    fn move_from_painted_end_to(
        &self,
        stdout: &mut std::io::Stdout,
        target_row: usize,
        target_col: usize,
    ) -> Result<()> {
        let (input_end_row, _) = self.end_position();
        let total_end_row = input_end_row + self.painted_menu_rows;
        queue!(stdout, MoveToColumn(0))?;
        if total_end_row > 0 {
            queue!(stdout, MoveUp(saturating_u16(total_end_row)))?;
        }
        if target_row > 0 {
            queue!(stdout, MoveDown(saturating_u16(target_row)))?;
        }
        queue!(stdout, MoveToColumn(saturating_u16(target_col)))?;
        Ok(())
    }

    /// Visual (row, col) the cursor would land at if we wrote
    /// `prompt + buffer[0..end]` from a fresh prompt-start position.
    /// Walks the buffer one character at a time so explicit `\n` chars
    /// produce hard row breaks (col = 0) and characters that would push
    /// past `columns` get a soft-wrap row break.
    fn position_at(&self, end: usize) -> (usize, usize) {
        let mut row = 0usize;
        let mut col = self.prompt_width;
        for ch in self.buffer.iter().take(end) {
            if *ch == '\n' {
                row += 1;
                col = 0;
                continue;
            }
            let w = UnicodeWidthChar::width(*ch).unwrap_or(0);
            if self.columns > 0 && col + w > self.columns {
                row += 1;
                col = 0;
            }
            col += w;
        }
        (row, col)
    }

    /// Position the user's cursor should sit at after a `redraw`.
    fn cursor_position(&self) -> (usize, usize) {
        self.position_at(self.cursor)
    }

    /// Position just past the buffer's end + the ghost suffix. The ghost
    /// renders on the same row the buffer ends in (it's a hint, not
    /// committed text); only soft-wrap can push it down.
    fn end_position(&self) -> (usize, usize) {
        let (mut row, mut col) = self.position_at(self.buffer.len());
        let ghost = self.painted_ghost_width;
        if self.columns > 0 && col + ghost > self.columns {
            row += ghost.div_ceil(self.columns.max(1));
            col = ghost % self.columns;
        } else {
            col += ghost;
        }
        (row, col)
    }

    #[cfg(test)]
    fn buffer_width_until(&self, end: usize) -> usize {
        self.buffer
            .iter()
            .take(end)
            .map(|ch| UnicodeWidthChar::width(*ch).unwrap_or(0))
            .sum()
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
    fn editor_tracks_cursor_position_separately_from_buffer_end() {
        let mut editor = LineEditor::new(80, Vec::new());
        editor.insert('物');
        editor.insert('a');
        editor.insert('品');
        assert!(editor.move_left());

        assert_eq!(editor.input(), "物a品");
        let prompt_w = UnicodeWidthStr::width(output::prompt_plain());
        assert_eq!(editor.cursor_position(), (0, prompt_w + 3));
        assert_eq!(editor.end_position(), (0, prompt_w + 5));
    }

    #[test]
    fn editor_maps_wrapped_columns_by_display_width() {
        // Prompt eats `prompt_w` columns of row 0; with cols=10 the
        // remaining 10 - prompt_w cells fit on row 0 before the cursor
        // wraps. A 2-wide CJK character that would straddle the row
        // boundary instead lands wholly on row 1 — terminals never
        // split a wide cell across rows, and the math now agrees.
        let mut editor = LineEditor::new(10, Vec::new());
        editor.insert('a');
        editor.insert('物');

        assert_eq!(editor.cursor_position(), (1, 2));
        editor.insert('品');
        assert_eq!(editor.cursor_position(), (1, 4));
    }

    #[test]
    fn insert_str_pastes_multi_line_block_as_one_buffer() {
        let mut editor = LineEditor::new(80, Vec::new());
        editor.insert_str("line 1\nline 2\nline 3");
        // The paste lands as three logical rows, separated by `\n` in
        // the buffer — Enter would dispatch all three lines together.
        assert_eq!(editor.input(), "line 1\nline 2\nline 3");
        assert_eq!(editor.cursor, editor.buffer.len());
    }

    #[test]
    fn insert_str_normalises_crlf_from_windows_pastes() {
        let mut editor = LineEditor::new(80, Vec::new());
        editor.insert_str("a\r\nb\rc");
        // \r\n collapses to \n; bare \r also becomes \n. So the buffer
        // never carries platform-specific line-ending differences past
        // this point.
        assert_eq!(editor.input(), "a\nb\nc");
    }

    #[test]
    fn insert_str_drops_control_chars_but_keeps_tabs() {
        let mut editor = LineEditor::new(80, Vec::new());
        editor.insert_str("ok\x07bell\tindent");
        assert_eq!(editor.input(), "okbell\tindent");
    }

    #[test]
    fn cursor_position_treats_newline_as_row_break() {
        let mut editor = LineEditor::new(80, Vec::new());
        editor.insert_str("abc\ndef");
        // Cursor sits at the end of "def" on the second row, column 3.
        // Without `\n` awareness the math would say (0, prompt_w + 7).
        assert_eq!(editor.cursor_position(), (1, 3));
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
        assert_eq!(editor.accept_completion(), AcceptOutcome::ExpectsMore);
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
        assert_eq!(editor.accept_completion(), AcceptOutcome::None);
        assert_eq!(editor.input(), "ping @ci_status");
    }

    #[test]
    fn accept_completion_signals_expects_more_for_role_mentions() {
        // The user's complaint: `@host<Enter>` used to dispatch a
        // turn with an empty body because Enter accepted the ghost
        // and submitted in one stroke. The fix: accept_completion
        // returns ExpectsMore for role mentions so the Enter handler
        // confirms the mention and waits for the task body.
        let mut editor = role_editor();
        editor.insert('@');
        editor.insert('h');
        assert_eq!(editor.accept_completion(), AcceptOutcome::ExpectsMore);
        assert_eq!(editor.input(), "@host ");
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
        // SLASH_COMMANDS in alphabetical order: allow, compact, deny,
        // exit, halt, help, host, journal, patch, refresh, stop,
        // transcript, welcome.
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
        // ExpectsMore so Enter confirms the command and waits for the
        // tool-name argument, instead of submitting `/allow ` (which
        // would fall through to /help in the parser).
        assert_eq!(editor.accept_completion(), AcceptOutcome::ExpectsMore);
        assert_eq!(editor.input(), "/allow ");
        assert_eq!(editor.cursor, editor.buffer.len());
    }

    #[test]
    fn slash_accept_omits_space_for_halt_so_enter_halts_everything() {
        // `/halt` alone halts every running role; that is the common
        // path. Tab-accept must leave the buffer ready for an immediate
        // Enter, not pad a space the user has to backspace away.
        // Complete so Enter submits without a second keystroke.
        let mut editor = slash_editor();
        for ch in "/ha".chars() {
            editor.insert(ch);
        }
        assert_eq!(editor.accept_completion(), AcceptOutcome::Complete);
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
        // /exit takes no args; accept lands the cursor right after `t`
        // and signals Complete so Enter dispatches the command directly.
        assert_eq!(editor.accept_completion(), AcceptOutcome::Complete);
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
        assert_eq!(editor.accept_completion(), AcceptOutcome::None);
    }

    // ---- dropdown menu ------------------------------------------------

    fn menu_editor() -> LineEditor {
        // Three roles with overlapping prefixes for `@b`.
        LineEditor::new(
            80,
            vec!["backend".into(), "backstage".into(), "batch".into()],
        )
    }

    /// Strip ANSI escape sequences for content-based assertions on
    /// styled menu rows.
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

    #[test]
    fn menu_hidden_when_fewer_than_two_matches() {
        let mut editor = menu_editor();
        for ch in "@bac".chars() {
            editor.insert(ch);
        }
        // `bac` still matches `backend` and `backstage` — two matches,
        // menu opens. Confirm count.
        assert_eq!(editor.menu_rows().len(), 2);
        editor.insert('k');
        // `back` still matches both.
        assert_eq!(editor.menu_rows().len(), 2);
        editor.insert('e');
        // `backe` only matches `backend` → menu closes, ghost only.
        assert!(editor.menu_rows().is_empty());
        assert!(editor.ghost_suffix().is_some());
    }

    #[test]
    fn menu_marks_selected_row_with_leader() {
        let mut editor = menu_editor();
        editor.insert('@');
        editor.insert('b');
        // 3 matches: backend (selected, idx 0), backstage, batch.
        let rows = editor.menu_rows();
        assert_eq!(rows.len(), 3);
        let plain: Vec<String> = rows.iter().map(|r| strip_ansi(r)).collect();
        assert!(plain[0].starts_with("▎ "));
        assert!(plain[0].contains("backend"));
        assert!(plain[1].starts_with("  "));
        assert!(plain[1].contains("backstage"));
    }

    #[test]
    fn down_arrow_advances_selection_and_up_arrow_reverses() {
        let mut editor = menu_editor();
        editor.insert('@');
        editor.insert('b');
        // First down: index 0 -> 1 (backstage).
        assert!(editor.cycle_completion());
        let after_down = strip_ansi(&editor.menu_rows()[1]);
        assert!(after_down.starts_with("▎ "));
        // Up should walk back to index 0 (backend).
        assert!(editor.cycle_completion_back());
        let after_up = strip_ansi(&editor.menu_rows()[0]);
        assert!(after_up.starts_with("▎ "));
    }

    #[test]
    fn up_arrow_wraps_around_at_top() {
        let mut editor = menu_editor();
        editor.insert('@');
        editor.insert('b');
        // From the freshly opened cycle, Up should wrap to the last
        // candidate (index 2 = batch).
        assert!(editor.cycle_completion_back());
        let rows = editor.menu_rows();
        let plain: Vec<String> = rows.iter().map(|r| strip_ansi(r)).collect();
        assert!(plain[2].starts_with("▎ "));
        assert!(plain[2].contains("batch"));
    }

    #[test]
    fn esc_hides_menu_until_prefix_changes() {
        let mut editor = menu_editor();
        editor.insert('@');
        editor.insert('b');
        assert_eq!(editor.menu_rows().len(), 3);
        // Esc hides the menu for this prefix; the ghost stays.
        assert!(editor.dismiss_completion());
        assert!(editor.menu_rows().is_empty());
        // Tab should NOT bring the menu back — the user just said no.
        editor.cycle_completion();
        assert!(editor.menu_rows().is_empty());
        // Typing a new character resets the dismissal because the
        // prefix changed; the menu re-opens with the new candidate set.
        editor.insert('a');
        // `@ba` matches backend, backstage, batch — still 3.
        assert_eq!(editor.menu_rows().len(), 3);
    }

    #[test]
    fn slash_menu_shows_description_column() {
        let mut editor = LineEditor::new(80, Vec::new());
        editor.insert('/');
        editor.insert('h');
        // /h matches /halt, /help, /host — three rows.
        let rows = editor.menu_rows();
        assert_eq!(rows.len(), 3);
        let plain: Vec<String> = rows.iter().map(|r| strip_ansi(r)).collect();
        assert!(plain[0].contains("/halt"));
        // Description column reproduces the table entry verbatim.
        assert!(plain[0].contains("interrupt the current turn"));
    }

    #[test]
    fn menu_overflow_footer_when_more_than_max_visible() {
        // Spin up enough roles to overflow MENU_MAX_VISIBLE.
        let names: Vec<String> = (0..MENU_MAX_VISIBLE + 3)
            .map(|i| format!("alpha{i:02}"))
            .collect();
        let mut editor = LineEditor::new(80, names);
        editor.insert('@');
        editor.insert('a');
        let rows = editor.menu_rows();
        assert_eq!(rows.len(), MENU_MAX_VISIBLE + 1);
        let footer = strip_ansi(rows.last().unwrap());
        assert!(footer.contains("+3 more"));
    }

    #[test]
    fn menu_hidden_when_no_active_token() {
        let mut editor = menu_editor();
        for ch in "plain text".chars() {
            editor.insert(ch);
        }
        assert!(editor.menu_rows().is_empty());
    }
}
