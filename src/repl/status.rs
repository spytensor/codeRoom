use std::io::{IsTerminal, Write as _};
use std::time::{Duration, Instant};

use crossterm::{style::Stylize, terminal};

use crate::crep::CrepEvent;
use crate::output;

use super::render::summarize_tool_input;

/// Frames of the standard braille spinner. ~10 frames at 100 ms gives
/// a familiar one-second rotation that matches `cargo`, `npm`, etc.
pub(super) const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Tick interval for the spinner, in milliseconds. Below ~80 ms users
/// notice the redraws as flicker; above ~120 ms it looks frozen.
pub(super) const SPINNER_TICK_MS: u64 = 100;

/// Status line region that lives below the user's last input while we wait
/// for role activity. Today it owns one role slot; the type boundary is the
/// future contract for concurrent multi-role rendering.
///
/// Skips all output when stdout is not a TTY (`cr ... | tee log.txt`)
/// to keep redirected output free of ANSI escapes.
pub(super) struct StatusRegion {
    pub(super) slots: Vec<StatusSlot>,
    pub(super) is_painted: bool,
    pub(super) is_tty: bool,
}

#[derive(Debug, Clone)]
pub(super) struct StatusSlot {
    pub(super) role: String,
    pub(super) frame: usize,
    /// Wall-clock when the role's turn started — feeds the elapsed
    /// readout in the status line.
    pub(super) started_at: Instant,
    /// Number of `ToolCallProposed` events observed for this turn.
    /// Surfaces in the status line as `… · N tools …` so the user
    /// has a sense of progress beyond just elapsed time.
    pub(super) tools_seen: usize,
    /// Best-effort label for what the role is doing right now.
    /// `None` between events ⇒ rendered as "thinking".
    pub(super) current_state: Option<String>,
}

impl StatusRegion {
    pub(super) fn start(role: &str) -> Self {
        Self::start_at(role, Instant::now())
    }

    pub(super) fn start_at(role: &str, started_at: Instant) -> Self {
        let mut region = Self {
            slots: vec![StatusSlot {
                role: role.to_owned(),
                frame: 0,
                started_at,
                tools_seen: 0,
                current_state: None,
            }],
            is_painted: false,
            is_tty: std::io::stdout().is_terminal(),
        };
        region.repaint();
        region
    }

    /// Update slot metadata from a CREP event so the status line can
    /// render `… · 12 tools · running Bash` instead of a flat
    /// "working" placeholder.
    pub(super) fn update_from_event(&mut self, event: &CrepEvent) {
        let Some(slot) = self.slots.first_mut() else {
            return;
        };
        match event {
            CrepEvent::ToolCallProposed {
                role,
                tool_name,
                tool_input,
                ..
            } if role == &slot.role => {
                slot.tools_seen = slot.tools_seen.saturating_add(1);
                let summary = summarize_tool_input(tool_input);
                slot.current_state = Some(if summary.trim().is_empty() {
                    format!("running {tool_name}")
                } else {
                    format!("running {tool_name} {summary}")
                });
            }
            CrepEvent::ToolCallExecuted { role, .. } if role == &slot.role => {
                slot.current_state = Some("thinking".to_owned());
            }
            CrepEvent::PermissionDenied {
                role, tool_name, ..
            } if role == &slot.role => {
                slot.current_state = Some(format!("denied {tool_name}"));
            }
            _ => {}
        }
    }

    pub(super) fn mark_waiting_approval(
        &mut self,
        role: &str,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.role == role) else {
            return;
        };
        let summary = summarize_tool_input(tool_input);
        slot.current_state = Some(if summary.trim().is_empty() {
            format!("waiting approval · {tool_name}")
        } else {
            format!("waiting approval · {tool_name} {summary}")
        });
    }

    pub(super) fn clear_waiting_approval(&mut self, role: &str) {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.role == role) else {
            return;
        };
        if slot
            .current_state
            .as_deref()
            .is_some_and(|state| state.starts_with("waiting approval"))
        {
            slot.current_state = Some("thinking".to_owned());
        }
    }

    fn paint(&mut self) {
        if !self.is_tty {
            return;
        }
        // \r returns cursor to col 0; \x1b[2K clears the whole line.
        // The role color is dropped on intentionally so the line is
        // unambiguously "status" and not confused with a RoleSpoke.
        let columns = terminal::size().map_or(80, |(cols, _)| usize::from(cols));
        print!("\r\x1b[2K{}", self.render_line_at_width(columns));
        let _ = std::io::stdout().flush();
        self.is_painted = true;
    }

    pub(super) fn advance(&mut self) {
        for slot in &mut self.slots {
            slot.frame = (slot.frame + 1) % SPINNER_FRAMES.len();
        }
        if self.is_painted {
            self.paint();
        }
    }

    pub(super) fn repaint(&mut self) {
        self.paint();
    }

    pub(super) fn clear(&mut self) {
        if !self.is_tty || !self.is_painted {
            self.is_painted = false;
            return;
        }
        print!("\r\x1b[2K");
        let _ = std::io::stdout().flush();
        self.is_painted = false;
    }

    pub(super) fn render_line_at_width(&self, width: usize) -> String {
        let slots = self
            .slots
            .iter()
            .map(|slot| {
                let frame = SPINNER_FRAMES[slot.frame % SPINNER_FRAMES.len()];
                let elapsed = format_short_duration(slot.started_at.elapsed());
                let tools = match slot.tools_seen {
                    0 => String::new(),
                    1 => " · 1 tool".to_owned(),
                    n => format!(" · {n} tools"),
                };
                let state = slot.current_state.as_deref().unwrap_or("thinking");
                format!(
                    "{frame} @{role} · {elapsed}{tools} · {state}",
                    role = slot.role,
                )
            })
            .collect::<Vec<_>>()
            .join("  ");
        let count = self.slots.len();
        let noun = if count == 1 { "role" } else { "roles" };
        let line = format!("│ {count} {noun} working · {slots}");
        output::truncate_visible(&line, width)
            .with(output::DIM)
            .to_string()
    }
}

/// Compact wall-clock readout for the status line.  `<60s` shows
/// seconds; `<60m` shows whole minutes; otherwise hours-and-minutes.
/// Stays consistent across screen redraws (no fractional seconds) so
/// the spinner doesn't churn the line on every tick.
fn format_short_duration(elapsed: Duration) -> String {
    let total = elapsed.as_secs();
    if total < 60 {
        format!("{total}s")
    } else if total < 3600 {
        format!("{}m", total / 60)
    } else {
        let hours = total / 3600;
        let mins = (total % 3600) / 60;
        if mins == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {mins}m")
        }
    }
}

impl Drop for StatusRegion {
    fn drop(&mut self) {
        // Defensive: never leave status text painted on the screen if a
        // panic or early return ate the explicit clear() call.
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_duration_under_a_minute_shows_seconds() {
        assert_eq!(format_short_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_short_duration(Duration::from_secs(12)), "12s");
        assert_eq!(format_short_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn short_duration_under_an_hour_shows_whole_minutes() {
        assert_eq!(format_short_duration(Duration::from_secs(60)), "1m");
        assert_eq!(format_short_duration(Duration::from_secs(120)), "2m");
        assert_eq!(format_short_duration(Duration::from_secs(3_599)), "59m");
    }

    #[test]
    fn short_duration_over_an_hour_shows_hours_and_minutes() {
        assert_eq!(format_short_duration(Duration::from_secs(3_600)), "1h");
        assert_eq!(format_short_duration(Duration::from_secs(3_660)), "1h 1m");
        assert_eq!(format_short_duration(Duration::from_secs(7_320)), "2h 2m");
        // 26h is plausible for a long-running automation; the format
        // doesn't degrade.
        assert_eq!(format_short_duration(Duration::from_secs(93_600)), "26h");
    }
}
