//! The interactive REPL.
//!
//! `cr start` enters this loop. v0.1 is intentionally synchronous: each
//! user input picks exactly one role, sends it a prompt, and renders bus
//! events until that role emits a `RoleSpoke` (its turn is done). Then
//! the loop re-prompts.
//!
//! Cross-role auto-routing (when one role writes `@x` in its reply) and
//! concurrent role rendering are deferred to a follow-up PR.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use crossterm::style::Stylize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::adapter::cc::CcAdapter;
use crate::adapter::{Engine, EngineAdapter, RoleHandle, UserMessage};
use crate::bus::MessageBus;
use crate::config::{Config, CODEROOM_DIR};
use crate::crep::CrepEvent;

/// One parsed user input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Address a specific role with `@<role> <text>`.
    SendTo {
        /// Role name from the `@<role>` prefix.
        role: String,
        /// Free-form prompt text.
        text: String,
    },
    /// Bare text — routed to the configured host role.
    SendToHost(String),
    /// `/stop <role>` — terminate the named role's subprocess.
    Stop(String),
    /// `/help` — print the help banner.
    Help,
    /// `/exit` or empty input on EOF — leave the REPL.
    Exit,
    /// Empty input — re-prompt without doing anything.
    Empty,
}

/// Parse one line of user input. Pure function — no I/O.
#[must_use]
pub fn parse_line(input: &str) -> Command {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Command::Empty;
    }
    if let Some(rest) = trimmed.strip_prefix('/') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        return match cmd {
            "exit" | "quit" => Command::Exit,
            "help" | "h" => Command::Help,
            "stop" if !arg.is_empty() => Command::Stop(arg.to_owned()),
            _ => Command::Help, // unknown slash command → show help
        };
    }
    if let Some(rest) = trimmed.strip_prefix('@') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let role = parts.next().unwrap_or("").to_owned();
        let text = parts.next().unwrap_or("").trim().to_owned();
        if !role.is_empty() && !text.is_empty() {
            return Command::SendTo { role, text };
        }
    }
    Command::SendToHost(trimmed.to_owned())
}

/// Live state for a single running role inside the REPL.
struct RunningRole {
    tx_user: mpsc::Sender<UserMessage>,
}

/// REPL entry point. Loads config, spawns every declared role, forwards
/// each role's events into the bus, then enters the line-mode loop.
pub async fn run(project_root: &Path) -> Result<()> {
    let cfg = Config::load(project_root)
        .with_context(|| format!("loading config in {project_root:?}"))?;
    let coderoom_dir = project_root.join(CODEROOM_DIR);

    let log_path = coderoom_dir.join("messages.jsonl");
    let bus = Arc::new(MessageBus::open(&log_path).await?);

    let cc_adapter = CcAdapter::new();

    let mut roles: HashMap<String, RunningRole> = HashMap::new();
    for name in cfg
        .role_names()
        .map(ToOwned::to_owned)
        .collect::<Vec<String>>()
    {
        let role_cfg = cfg
            .role_config(&name, &coderoom_dir)
            .expect("role declared but role_config returned None");
        let handle = match role_cfg.engine {
            Engine::Cc => cc_adapter
                .start(role_cfg)
                .await
                .with_context(|| format!("spawning role `{name}`"))?,
            Engine::Codex | Engine::Gemini => {
                bail!(
                    "engine `{}` is not yet supported in v0.1 — only `cc` is implemented",
                    role_cfg.engine.as_str(),
                );
            }
        };

        // Split the handle: events get forwarded to the bus in a background
        // task; the user-message sender goes into the REPL's roles map.
        let RoleHandle {
            role: rname,
            engine: _,
            tx_user,
            rx_events,
        } = handle;
        spawn_event_forwarder(rname.clone(), rx_events, Arc::clone(&bus));
        roles.insert(rname, RunningRole { tx_user });
    }

    print_banner(&cfg);

    let mut renderer_rx = bus.subscribe();
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    loop {
        prompt(&mut stdout).await?;
        let Some(line) = stdin.next_line().await? else {
            break;
        };
        match parse_line(&line) {
            Command::Empty => continue,
            Command::Exit => break,
            Command::Help => {
                print_help(&cfg);
                continue;
            }
            Command::Stop(role) => {
                if let Some(running) = roles.remove(&role) {
                    drop(running.tx_user);
                    println!("{}", format!("[stopped @{role}]").dim());
                } else {
                    println!("{}", format!("no such role: @{role}").red());
                }
            }
            Command::SendTo { role, text } => {
                send_and_drain(&roles, &mut renderer_rx, &role, &text).await?;
            }
            Command::SendToHost(text) => {
                let host = cfg.host_role.clone();
                send_and_drain(&roles, &mut renderer_rx, &host, &text).await?;
            }
        }
    }

    Ok(())
}

async fn prompt(stdout: &mut tokio::io::Stdout) -> Result<()> {
    stdout.write_all(b"\n> ").await?;
    stdout.flush().await?;
    Ok(())
}

fn print_banner(cfg: &Config) {
    let mut roles: Vec<&str> = cfg.role_names().collect();
    roles.sort_unstable();
    let host = &cfg.host_role;
    println!(
        "cr {} — roles: {} (host: @{host})",
        env!("CARGO_PKG_VERSION"),
        roles
            .iter()
            .map(|r| format!("@{r}"))
            .collect::<Vec<_>>()
            .join(" "),
    );
    println!("type @<role> <message>; bare text routes to @{host}; /help, /exit");
}

fn print_help(cfg: &Config) {
    println!("commands:");
    println!("  @<role> <text>   send to a specific role");
    println!("  <text>           send to host (@{})", cfg.host_role);
    println!("  /stop <role>     terminate a role's subprocess");
    println!("  /help            this help");
    println!("  /exit, /quit     leave the REPL");
}

async fn send_and_drain(
    roles: &HashMap<String, RunningRole>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    role: &str,
    text: &str,
) -> Result<()> {
    let Some(running) = roles.get(role) else {
        println!("{}", format!("no such role: @{role}").red());
        return Ok(());
    };

    if let Err(error) = running
        .tx_user
        .send(UserMessage::Prompt(text.to_owned()))
        .await
    {
        warn!(role, %error, "user-message channel for role closed");
        return Ok(());
    }

    // Drain bus events until the addressed role finishes its turn.
    loop {
        match rx.recv().await {
            Ok(event) => {
                render_event(&event);
                if let CrepEvent::RoleSpoke { role: spoken, .. } = &event {
                    if spoken == role {
                        break;
                    }
                }
                if matches!(&event, CrepEvent::RoleStopped { role: stopped, .. } if stopped == role)
                {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                println!(
                    "{}",
                    format!("[renderer fell behind, skipped {skipped} event(s)]").dim()
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
    Ok(())
}

fn render_event(event: &CrepEvent) {
    match event {
        CrepEvent::RoleStarted { role, model, .. } => {
            println!(
                "{}",
                format!("[@{role} ready · model={model}]").dim().italic()
            );
        }
        CrepEvent::RoleSpoke {
            role,
            text,
            cost_usd,
            ..
        } => {
            println!(
                "{} {}",
                format!("@{role}").with(role_color(role)).bold(),
                text,
            );
            debug!(role, cost_usd, "RoleSpoke rendered");
        }
        CrepEvent::ToolCallProposed {
            role,
            tool_name,
            tool_input,
            ..
        } => {
            let summary = summarize_tool_input(tool_input);
            println!("{}", format!("  ↳ @{role} · {tool_name} {summary}").dim());
        }
        CrepEvent::ToolCallExecuted {
            role,
            ok,
            output_summary,
            ..
        } => {
            let glyph = if *ok { "✓" } else { "✗" };
            println!("{}", format!("  {glyph} @{role} · {output_summary}").dim());
        }
        CrepEvent::PermissionDenied {
            role,
            tool_name,
            reason,
            ..
        } => {
            println!(
                "{}",
                format!("  ⊘ @{role} · {tool_name} denied: {reason}").yellow()
            );
        }
        CrepEvent::RoleStopped { role, reason } => {
            println!(
                "{}",
                format!("[@{role} stopped: {reason:?}]").dim().italic()
            );
        }
    }
}

/// Stable per-role color so the same role keeps its color across the
/// whole session. Hash the role name into the 256-color palette,
/// avoiding the dim/black region.
fn role_color(role: &str) -> crossterm::style::Color {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    role.hash(&mut hasher);
    // Map into a curated set of bright, distinguishable colors.
    let palette = [
        crossterm::style::Color::Cyan,
        crossterm::style::Color::Magenta,
        crossterm::style::Color::Yellow,
        crossterm::style::Color::Green,
        crossterm::style::Color::Blue,
        crossterm::style::Color::Red,
    ];
    palette[(hasher.finish() as usize) % palette.len()]
}

fn summarize_tool_input(input: &serde_json::Value) -> String {
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

fn truncate_inline(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Forward all events from a role's `rx_events` into the shared bus.
fn spawn_event_forwarder(role: String, mut rx: mpsc::Receiver<CrepEvent>, bus: Arc<MessageBus>) {
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let Err(error) = bus.publish(event).await {
                warn!(role, %error, "failed to publish event to bus");
            }
        }
        debug!(role, "event forwarder exiting");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_empty_input_yields_empty_command() {
        assert_eq!(parse_line(""), Command::Empty);
        assert_eq!(parse_line("   "), Command::Empty);
        assert_eq!(parse_line("\n"), Command::Empty);
    }

    #[test]
    fn parse_at_mention_routes_to_role() {
        match parse_line("@backend please read the auth module") {
            Command::SendTo { role, text } => {
                assert_eq!(role, "backend");
                assert_eq!(text, "please read the auth module");
            }
            other => panic!("expected SendTo, got {other:?}"),
        }
    }

    #[test]
    fn parse_bare_text_routes_to_host() {
        match parse_line("any free-form text here") {
            Command::SendToHost(text) => assert_eq!(text, "any free-form text here"),
            other => panic!("expected SendToHost, got {other:?}"),
        }
    }

    #[test]
    fn parse_at_with_no_text_falls_back_to_host() {
        // "@backend" alone is treated as bare text, not a routing command,
        // because there's nothing to send. The host gets the literal "@backend".
        match parse_line("@backend") {
            Command::SendToHost(text) => assert_eq!(text, "@backend"),
            other => panic!("expected SendToHost, got {other:?}"),
        }
    }

    #[test]
    fn parse_slash_exit_quit_help() {
        assert_eq!(parse_line("/exit"), Command::Exit);
        assert_eq!(parse_line("/quit"), Command::Exit);
        assert_eq!(parse_line("/help"), Command::Help);
        assert_eq!(parse_line("/h"), Command::Help);
    }

    #[test]
    fn parse_slash_stop_with_role() {
        assert_eq!(parse_line("/stop backend"), Command::Stop("backend".into()));
    }

    #[test]
    fn parse_slash_stop_without_role_shows_help() {
        // Defensively show help rather than tearing down something arbitrary.
        assert_eq!(parse_line("/stop"), Command::Help);
    }

    #[test]
    fn parse_unknown_slash_shows_help() {
        assert_eq!(parse_line("/whatever"), Command::Help);
    }

    #[test]
    fn parse_preserves_internal_whitespace() {
        match parse_line("@be   hello   world  ") {
            Command::SendTo { role, text } => {
                assert_eq!(role, "be");
                assert_eq!(text, "hello   world");
            }
            other => panic!("expected SendTo, got {other:?}"),
        }
    }

    #[test]
    fn role_color_is_stable_for_same_name() {
        assert_eq!(role_color("backend"), role_color("backend"));
        assert_eq!(role_color("frontend"), role_color("frontend"));
    }

    #[test]
    fn summarize_tool_input_shows_bash_command() {
        let v = serde_json::json!({"command": "ls -la"});
        let s = summarize_tool_input(&v);
        assert!(s.contains("ls -la"));
    }

    #[test]
    fn summarize_tool_input_shows_file_path() {
        let v = serde_json::json!({"file_path": "src/main.rs"});
        assert_eq!(summarize_tool_input(&v), "src/main.rs");
    }

    #[test]
    fn summarize_tool_input_falls_back_to_keys() {
        let v = serde_json::json!({"foo": 1, "bar": 2});
        let s = summarize_tool_input(&v);
        // HashMap iteration order isn't stable; just check both keys appear.
        assert!(s.contains("foo"));
        assert!(s.contains("bar"));
    }

    #[test]
    fn truncate_inline_truncates_long_strings() {
        let out = truncate_inline("0123456789abcdef", 8);
        assert_eq!(out.chars().count(), 8);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_inline_preserves_short_strings() {
        assert_eq!(truncate_inline("hi", 8), "hi");
    }
}
