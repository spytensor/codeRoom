//! Centralized output styling.
//!
//! Non-raw-mode user-facing prints in `repl.rs` route through here so the
//! palette and semantic rules live in one place. Raw-mode renderers (the
//! `init` wizard, the in-place thinking spinner, the `UiCell` row builders)
//! consume the constants and fragment helpers but draw their own frames —
//! see `docs/colors.md` §6 / §8 for the carve-outs and rationale.
//!
//! Anything semantic (status messages, role tokens, command tokens, tool
//! traces) should call into this module rather than reaching for a raw
//! `crossterm` color. That way swapping a palette entry later touches one
//! file instead of forty.

use std::io::IsTerminal;

use crossterm::style::{Color, StyledContent, Stylize};

mod palette;

use palette::ROLE_PALETTE;
pub use palette::{
    BAD, DIM, EM, FADE, INFO, KEY, MUTE, OK, PROMPT, RULE, SPLASH_ACCENT, SPLASH_FRAME,
    SPLASH_PILL_FG, SPLASH_VERSION, TEXT, WARN,
};

// ───────────────────── role palette ────────────────────────

/// FNV-1a 32-bit. Stable across Rust toolchain versions, unlike
/// `std::hash::DefaultHasher`. Five lines, no dependency.
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Stable role color. The host role is always lavender; every other role
/// hashes deterministically into slots 1..8. The same role name therefore
/// keeps the same color across sessions and across Rust versions.
#[must_use]
pub fn role_color(role: &str, host_role: &str) -> Color {
    if role == "host" || role == host_role {
        return ROLE_PALETTE[0];
    }
    let idx = 1 + (fnv1a(role) as usize) % 7;
    ROLE_PALETTE[idx]
}

// ───────────────────── status helpers ──────────────────────

/// `✓ <msg>` in success colors.
pub fn ok(msg: impl AsRef<str>) {
    println!("{} {}", "✓".with(OK), msg.as_ref().with(TEXT));
}

/// `⚠ <msg>` in warning colors.
pub fn warn(msg: impl AsRef<str>) {
    println!("{} {}", "⚠".with(WARN), msg.as_ref().with(TEXT));
}

/// `✗ <msg>` in failure colors.
pub fn bad(msg: impl AsRef<str>) {
    println!("{} {}", "✗".with(BAD), msg.as_ref().with(TEXT));
}

/// Indented secondary line that follows a primary status line. Color
/// steps down to `FADE` per the npm/cargo convention.
pub fn hint(msg: impl AsRef<str>) {
    println!("  {}", msg.as_ref().with(FADE));
}

/// `[<msg>]` in dimmed italic — system bracket convention used for
/// `[@role ready · model=...]` and `[@role stopped: ...]`.
pub fn system(msg: impl AsRef<str>) {
    println!("{}", format!("[{}]", msg.as_ref()).with(DIM).italic());
}

/// `  ↳ @role · <summary>` — tool-call trace. No timestamp by design;
/// tool events are too frequent for a per-line clock.
pub fn tool_trace(role: &str, summary: impl AsRef<str>) {
    println!(
        "  {} @{role} · {}",
        "↳".with(FADE),
        summary.as_ref().with(DIM),
    );
}

// ───────────────────── fragment helpers ────────────────────
//
// The `repl.rs` home dashboard builds rows out of `UiCell` values that
// pair a styled string with its visible character count. These helpers
// return styled fragments so those builders can continue to assemble
// their own lines without re-importing the palette directly.

/// `@<role>` styled with the role's color and bold.
#[must_use]
pub fn role_token(role: &str, host_role: &str) -> StyledContent<String> {
    format!("@{role}").with(role_color(role, host_role)).bold()
}

/// `●` styled with the role's color (used in the boot role list).
#[must_use]
pub fn role_dot(role: &str, host_role: &str) -> StyledContent<&'static str> {
    "●".with(role_color(role, host_role))
}

/// Style a command/hotkey token (`/help`, `/patch`).
#[must_use]
pub fn cmd(text: impl Into<String>) -> StyledContent<String> {
    text.into().with(KEY)
}

/// The `⚡ cr ›` prompt as a string. The caller controls placement and
/// flushing — this helper just owns the styling. The lightning bolt
/// matches the splash accent so the prompt feels of a piece with the
/// boot frame.
#[must_use]
pub fn prompt() -> String {
    format!(
        "\n{} {} ",
        "⚡".with(SPLASH_ACCENT),
        "cr ›".with(PROMPT).bold()
    )
}

// ───────────────────── typography helpers ─────────────────

/// Truncate `s` so it occupies at most `max_chars` visible cells,
/// appending `…` when truncation happens. Counts via `chars().count()`,
/// which matches the visible width for ASCII identifiers and the
/// glyphs used elsewhere in this module. Wide-East-Asian descriptions
/// would need `unicode-width`, but role names are ASCII-constrained at
/// validation time.
#[must_use]
pub fn truncate_visible(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let len = s.chars().count();
    if len <= max_chars {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ───────────────────── degradation ─────────────────────────

/// Whether colored output is appropriate in this process. v0.1 relies on
/// crossterm's own `should_colorize` behaviour for TTY detection; explicit
/// `NO_COLOR` plumbing is v0.1.x.
#[must_use]
pub fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if !std::io::stdout().is_terminal() {
        return false;
    }
    !matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
}

/// Print one startup diagnostics line to stderr so terminal color
/// reports can include whether truecolor is discoverable.
pub fn print_terminal_probe() {
    if terminal_probe_enabled() {
        eprintln!("{}", terminal_probe_line());
    }
}

fn terminal_probe_line() -> String {
    let term = std::env::var("TERM").unwrap_or_else(|_| "(unset)".to_owned());
    let colorterm = std::env::var("COLORTERM").unwrap_or_else(|_| "(unset)".to_owned());
    terminal_probe_line_from(&term, &colorterm)
}

fn terminal_probe_line_from(term: &str, colorterm: &str) -> String {
    let term_lower = term.to_ascii_lowercase();
    let colorterm_lower = colorterm.to_ascii_lowercase();
    let truecolor = matches!(colorterm_lower.as_str(), "truecolor" | "24bit")
        || term_lower.contains("truecolor")
        || term_lower.contains("24bit");
    format!(
        "coderoom terminal: TERM={term} COLORTERM={colorterm} truecolor={}",
        if truecolor { "yes" } else { "no" }
    )
}

fn terminal_probe_enabled() -> bool {
    std::env::var("CODEROOM_TERMINAL_PROBE").is_ok_and(|value| terminal_probe_enabled_from(&value))
}

fn terminal_probe_enabled_from(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_role_pins_to_lavender() {
        let host = role_color("host", "host");
        assert_eq!(host, ROLE_PALETTE[0]);
    }

    #[test]
    fn literal_host_is_always_lavender() {
        assert_eq!(role_color("host", "backend"), ROLE_PALETTE[0]);
    }

    #[test]
    fn role_color_is_stable_for_same_name() {
        // Same name ⇒ same color across calls.
        assert_eq!(role_color("backend", "host"), role_color("backend", "host"));
        assert_eq!(
            role_color("frontend", "host"),
            role_color("frontend", "host")
        );
    }

    #[test]
    fn role_color_differs_when_a_role_is_promoted_to_host() {
        // `backend` as a regular role hashes into slots 1..8.
        // Once `backend` becomes the host, it pins to lavender.
        let regular = role_color("backend", "host");
        let as_host = role_color("backend", "backend");
        assert_eq!(as_host, ROLE_PALETTE[0]);
        assert_ne!(regular, as_host);
    }

    #[test]
    fn fnv1a_matches_known_values() {
        // Public FNV-1a 32-bit reference vectors.
        assert_eq!(fnv1a(""), 0x811c_9dc5);
        assert_eq!(fnv1a("a"), 0xe40c_292c);
        assert_eq!(fnv1a("foobar"), 0xbf9c_f968);
    }

    #[test]
    fn non_host_roles_never_use_lavender() {
        // Slot 0 is reserved; only the host can land there.
        for name in [
            "backend", "frontend", "qa", "devops", "data", "security", "docs", "auth", "ingest",
            "platform", "ml", "infra",
        ] {
            assert_ne!(
                role_color(name, "host"),
                ROLE_PALETTE[0],
                "role `{name}` collided with the host slot"
            );
        }
    }

    #[test]
    fn terminal_probe_detects_truecolor_from_colorterm() {
        let line = terminal_probe_line_from("xterm-256color", "truecolor");
        assert!(line.contains("TERM=xterm-256color"));
        assert!(line.contains("COLORTERM=truecolor"));
        assert!(line.contains("truecolor=yes"));
    }

    #[test]
    fn terminal_probe_is_opt_in() {
        assert!(terminal_probe_enabled_from("1"));
        assert!(terminal_probe_enabled_from("true"));
        assert!(terminal_probe_enabled_from("on"));
        assert!(!terminal_probe_enabled_from(""));
        assert!(!terminal_probe_enabled_from("0"));
    }
}
