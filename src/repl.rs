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
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use crossterm::style::Stylize;
use tempfile::NamedTempFile;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::adapter::cc::CcAdapter;
use crate::adapter::codex::CodexAdapter;
use crate::adapter::gemini::GeminiAdapter;
use crate::adapter::{Engine, EngineAdapter, RoleHandle, UserMessage};
use crate::bus::MessageBus;
use crate::config::{Config, CODEROOM_DIR};
use crate::crep::CrepEvent;
use crate::priors;

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
    /// `/patch <role> <text>` — save a session-time correction for the
    /// named role. Persisted under `.coderoom/patches/<role>/`. Loaded
    /// on the role's next `/refresh` (or next `cr start`).
    Patch {
        /// Role whose priors will be patched.
        role: String,
        /// Correction text — written verbatim into the new patch file.
        text: String,
    },
    /// `/refresh <role>` — re-instantiate the role with the latest
    /// composed priors (shared.md + role.md + active patches). The
    /// old subprocess is dropped; a fresh one starts.
    Refresh(String),
    /// `/transcript <role>` — show the last few RoleSpoke entries for a
    /// role from `.coderoom/messages.jsonl`.
    Transcript(String),
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
            "stop" if !arg.is_empty() => Command::Stop(arg.to_owned()),
            "refresh" if !arg.is_empty() => {
                let role = arg.strip_prefix('@').unwrap_or(arg).to_owned();
                if role.is_empty() {
                    Command::Help
                } else {
                    Command::Refresh(role)
                }
            }
            "transcript" if !arg.is_empty() => {
                let role = arg.strip_prefix('@').unwrap_or(arg).to_owned();
                if role.is_empty() {
                    Command::Help
                } else {
                    Command::Transcript(role)
                }
            }
            "patch" => parse_patch_arg(arg).unwrap_or(Command::Help),
            // /help, /h, and any unknown slash command all fall through here.
            _ => Command::Help,
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

/// Parse the argument string of `/patch <role> <text>`. Accepts both
/// `backend foo bar` and `@backend foo bar` for ergonomics. Returns
/// `None` (caller falls back to Help) if either side is empty.
fn parse_patch_arg(arg: &str) -> Option<Command> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let role_token = parts.next().unwrap_or("");
    let text = parts.next().unwrap_or("").trim();
    let role = role_token
        .strip_prefix('@')
        .unwrap_or(role_token)
        .to_owned();
    if role.is_empty() || text.is_empty() {
        return None;
    }
    Some(Command::Patch {
        role,
        text: text.to_owned(),
    })
}

/// Bundle of every available engine adapter, constructed once per
/// `cr start` invocation. Bundled so REPL helpers don't have to
/// thread three separate references through every call site.
#[derive(Debug)]
struct Adapters {
    cc: CcAdapter,
    codex: CodexAdapter,
    gemini: GeminiAdapter,
}

impl Adapters {
    fn new() -> Self {
        Self {
            cc: CcAdapter::new(),
            codex: CodexAdapter::new(),
            gemini: GeminiAdapter::new(),
        }
    }
}

/// Live state for a single running role inside the REPL.
struct RunningRole {
    tx_user: mpsc::Sender<UserMessage>,
    /// Composed priors temp file. Held for the role's lifetime so the
    /// path passed to the engine via `--append-system-prompt-file`
    /// remains valid until the subprocess has fully read it. Dropped
    /// at role removal, which deletes the file.
    #[allow(
        dead_code,
        reason = "kept alive only for its Drop side-effect (tempfile cleanup)"
    )]
    priors_temp: NamedTempFile,
}

/// REPL entry point. Loads config, spawns every declared role, forwards
/// each role's events into the bus, then enters the line-mode loop.
pub async fn run(project_root: &Path) -> Result<()> {
    let cfg = Config::load(project_root)
        .with_context(|| format!("loading config in {project_root:?}"))?;
    let coderoom_dir = project_root.join(CODEROOM_DIR);

    let log_path = coderoom_dir.join("messages.jsonl");
    let bus = Arc::new(MessageBus::open(&log_path).await?);

    let adapters = Adapters::new();

    let mut roles: HashMap<String, RunningRole> = HashMap::new();
    for name in cfg
        .role_names()
        .map(ToOwned::to_owned)
        .collect::<Vec<String>>()
    {
        let running = spawn_role(&cfg, &adapters, &coderoom_dir, &name, &bus).await?;
        roles.insert(name, running);
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
            Command::Refresh(role) => {
                refresh_role(&cfg, &adapters, &coderoom_dir, &bus, &mut roles, &role).await;
            }
            Command::Transcript(role) => {
                show_transcript(&coderoom_dir, &role).await;
            }
            Command::Patch { role, text } => {
                if !cfg.roles.contains_key(&role) {
                    println!("{}", format!("no such role: @{role}").red());
                    continue;
                }
                match priors::write_patch(&coderoom_dir, &role, &text) {
                    Ok(outcome) => {
                        println!(
                            "{}",
                            format!("✓ patched @{role} → {}", outcome.path.display()).green()
                        );
                        if let Some(archived) = outcome.archived {
                            println!(
                                "{}",
                                format!(
                                    "  (cap reached; archived oldest → {})",
                                    archived.display()
                                )
                                .dim()
                            );
                        }
                        println!(
                            "{}",
                            "  applies to next /refresh; current session still uses old priors"
                                .dim()
                        );
                    }
                    Err(error) => {
                        println!("{}", format!("✗ patch failed: {error:#}").red());
                    }
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
    println!("  @<role> <text>      send to a specific role");
    println!("  <text>              send to host (@{})", cfg.host_role);
    println!("  /patch <role> <…>   save a correction; loads on next /refresh");
    println!("  /refresh <role>     re-instantiate role with latest priors+patches");
    println!("  /transcript <role>  show that role's recent spoken turns");
    println!("  /stop <role>        terminate a role's subprocess");
    println!("  /help               this help");
    println!("  /exit, /quit        leave the REPL");
}

/// Send `text` to `role` and drain bus events until that role finishes
/// its turn. If the role's final `RoleSpoke` mentions other running
/// roles, automatically forwards a brief to each (one hop only at v0.1
/// — multi-hop + hop-depth escalation are tracked in
/// `docs/proposed-amendments.md`).
async fn send_and_drain(
    roles: &HashMap<String, RunningRole>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    role: &str,
    text: &str,
) -> Result<()> {
    let Some(captured) = drain_one_turn(roles, rx, role, text).await? else {
        return Ok(());
    };

    // One-hop auto-routing: forward to each mentioned running role.
    // Self-references and unknown roles are skipped silently.
    for mention in &captured.mentions {
        if mention == role || !roles.contains_key(mention) {
            continue;
        }
        let brief = format!("From @{role}: {}", captured.text);
        println!(
            "{}",
            format!("  ↳ auto-routing to @{mention}").dim().italic()
        );
        if drain_one_turn(roles, rx, mention, &brief).await?.is_none() {
            break;
        }
    }
    Ok(())
}

/// Final assistant-turn fields captured during a single role's drain.
#[derive(Debug, Clone)]
struct CapturedTurn {
    text: String,
    mentions: Vec<String>,
}

/// Send `text` to `role` and drain bus events until that role's turn
/// ends. Returns the captured `RoleSpoke` info, or `None` if the role
/// stopped before producing a `RoleSpoke` (e.g., immediate crash).
///
/// All events are rendered along the way; this only returns to the
/// caller once the role's turn boundary is observed.
async fn drain_one_turn(
    roles: &HashMap<String, RunningRole>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    role: &str,
    text: &str,
) -> Result<Option<CapturedTurn>> {
    let Some(running) = roles.get(role) else {
        println!("{}", format!("no such role: @{role}").red());
        return Ok(None);
    };

    if let Err(error) = running
        .tx_user
        .send(UserMessage::Prompt(text.to_owned()))
        .await
    {
        warn!(role, %error, "user-message channel for role closed");
        return Ok(None);
    }

    let mut captured: Option<CapturedTurn> = None;
    loop {
        match rx.recv().await {
            Ok(event) => {
                render_event(&event);
                match &event {
                    CrepEvent::RoleSpoke {
                        role: spoken,
                        text,
                        mentions,
                        ..
                    } if spoken == role => {
                        captured = Some(CapturedTurn {
                            text: text.clone(),
                            mentions: mentions.clone(),
                        });
                        break;
                    }
                    CrepEvent::RoleStopped { role: stopped, .. } if stopped == role => break,
                    _ => {}
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
    Ok(captured)
}

/// Replay every event in `.coderoom/messages.jsonl` through the same
/// renderer the live REPL uses. Used by `cr show`.
pub async fn show_log(project_root: &Path) -> Result<()> {
    let coderoom_dir = project_root.join(CODEROOM_DIR);
    let log_path = coderoom_dir.join("messages.jsonl");
    if !log_path.is_file() {
        println!("(no messages — has `cr start` ever run in this project?)");
        return Ok(());
    }
    let events = MessageBus::replay(&log_path).await?;
    if events.is_empty() {
        println!("(message log is empty)");
        return Ok(());
    }
    for event in &events {
        render_event(event);
    }
    Ok(())
}

/// In-REPL: print the last few RoleSpoke events for `role` from the
/// active session's message log.
async fn show_transcript(coderoom_dir: &Path, role: &str) {
    const TAIL: usize = 5;
    let log_path = coderoom_dir.join("messages.jsonl");
    if !log_path.is_file() {
        println!("{}", "(no messages logged yet this session)".dim());
        return;
    }
    match MessageBus::replay(&log_path).await {
        Ok(events) => {
            let filtered: Vec<&CrepEvent> = events
                .iter()
                .filter(|e| matches!(e, CrepEvent::RoleSpoke { role: r, .. } if r == role))
                .collect();
            if filtered.is_empty() {
                println!("{}", format!("(no spoken turns from @{role} yet)").dim());
                return;
            }
            let start = filtered.len().saturating_sub(TAIL);
            println!(
                "{}",
                format!(
                    "── @{role}: last {} of {} turn(s) ──",
                    filtered.len() - start,
                    filtered.len()
                )
                .dim()
            );
            for event in &filtered[start..] {
                render_event(event);
            }
        }
        Err(error) => {
            println!("{}", format!("✗ failed to read message log: {error}").red());
        }
    }
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
    // The modular result is always in `0..palette.len()` (= 6), so the
    // `as usize` cast cannot truncate even on 32-bit pointer targets.
    #[allow(clippy::cast_possible_truncation)]
    let idx = (hasher.finish() % palette.len() as u64) as usize;
    palette[idx]
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

/// Drop the running role and re-spawn it with the freshly composed
/// priors. Validates that the role exists in the loaded config; prints
/// status to stdout via the same coloured channel as the rest of the
/// REPL.
async fn refresh_role(
    cfg: &Config,
    adapters: &Adapters,
    coderoom_dir: &Path,
    bus: &Arc<MessageBus>,
    roles: &mut HashMap<String, RunningRole>,
    role: &str,
) {
    if !cfg.roles.contains_key(role) {
        println!("{}", format!("no such role: @{role}").red());
        return;
    }
    if let Some(old) = roles.remove(role) {
        drop(old);
        println!("{}", format!("refreshing @{role}...").dim());
    }
    match spawn_role(cfg, adapters, coderoom_dir, role, bus).await {
        Ok(running) => {
            roles.insert(role.to_owned(), running);
            println!("{}", format!("✓ @{role} refreshed").green());
        }
        Err(error) => {
            println!(
                "{}",
                format!("✗ refreshing @{role} failed: {error:#}").red()
            );
        }
    }
}

/// Compose priors, stage them in a tempfile, spawn the role's
/// subprocess via the configured engine adapter, and wire its event
/// stream into `bus`. Returns the [`RunningRole`] the REPL should
/// keep alive.
async fn spawn_role(
    cfg: &Config,
    adapters: &Adapters,
    coderoom_dir: &Path,
    name: &str,
    bus: &Arc<MessageBus>,
) -> Result<RunningRole> {
    let composed = priors::compose_for(coderoom_dir, name)
        .with_context(|| format!("composing priors for role `{name}`"))?;
    let priors_temp = writepriors_tempfile(name, &composed)
        .with_context(|| format!("staging priors for role `{name}`"))?;

    let mut role_cfg = cfg
        .role_config(name, coderoom_dir)
        .expect("role declared but role_config returned None");
    priors_temp.path().clone_into(&mut role_cfg.priors_path);

    let handle = match role_cfg.engine {
        Engine::Cc => adapters
            .cc
            .start(role_cfg)
            .await
            .with_context(|| format!("spawning role `{name}`"))?,
        Engine::Codex => adapters
            .codex
            .start(role_cfg)
            .await
            .with_context(|| format!("spawning role `{name}` (codex)"))?,
        Engine::Gemini => adapters
            .gemini
            .start(role_cfg)
            .await
            .with_context(|| format!("spawning role `{name}` (gemini)"))?,
    };

    let RoleHandle {
        role: rname,
        engine: _,
        tx_user,
        rx_events,
    } = handle;
    spawn_event_forwarder(rname, rx_events, Arc::clone(bus));
    Ok(RunningRole {
        tx_user,
        priors_temp,
    })
}

/// Write composed priors to a tempfile in the system temp dir. The
/// tempfile name embeds the role for easier debugging when something
/// goes wrong. Caller is expected to keep the returned `NamedTempFile`
/// alive for as long as the engine subprocess might re-read the file.
fn writepriors_tempfile(role: &str, composed: &str) -> Result<NamedTempFile> {
    let mut tempfile = tempfile::Builder::new()
        .prefix(&format!("coderoom-priors-{role}-"))
        .suffix(".md")
        .tempfile()
        .context("creating priors tempfile")?;
    tempfile
        .write_all(composed.as_bytes())
        .context("writing composed priors")?;
    tempfile.flush().context("flushing composed priors")?;
    Ok(tempfile)
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
    fn parse_patch_with_role_and_text() {
        assert_eq!(
            parse_line("/patch backend rate limit goes in gateway config"),
            Command::Patch {
                role: "backend".into(),
                text: "rate limit goes in gateway config".into(),
            }
        );
    }

    #[test]
    fn parse_patch_accepts_at_prefixed_role() {
        assert_eq!(
            parse_line("/patch @backend use verify_token()"),
            Command::Patch {
                role: "backend".into(),
                text: "use verify_token()".into(),
            }
        );
    }

    #[test]
    fn parse_patch_without_text_shows_help() {
        assert_eq!(parse_line("/patch backend"), Command::Help);
    }

    #[test]
    fn parse_patch_without_role_shows_help() {
        assert_eq!(parse_line("/patch"), Command::Help);
    }

    #[test]
    fn parse_refresh_with_role() {
        assert_eq!(
            parse_line("/refresh backend"),
            Command::Refresh("backend".into())
        );
    }

    #[test]
    fn parse_refresh_accepts_at_prefixed_role() {
        assert_eq!(
            parse_line("/refresh @backend"),
            Command::Refresh("backend".into())
        );
    }

    #[test]
    fn parse_refresh_without_role_shows_help() {
        assert_eq!(parse_line("/refresh"), Command::Help);
    }

    #[test]
    fn parse_transcript_with_role() {
        assert_eq!(
            parse_line("/transcript backend"),
            Command::Transcript("backend".into())
        );
        assert_eq!(
            parse_line("/transcript @backend"),
            Command::Transcript("backend".into())
        );
    }

    #[test]
    fn parse_transcript_without_role_shows_help() {
        assert_eq!(parse_line("/transcript"), Command::Help);
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
