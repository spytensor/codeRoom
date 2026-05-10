//! The interactive REPL.
//!
//! `cr start` enters this loop. v0.1 is intentionally synchronous: each
//! user input picks exactly one role, sends it a prompt, and renders bus
//! events until that role emits a `RoleSpoke` (its turn is done). Then
//! the loop re-prompts.
//!
//! Cross-role auto-routing (when one role writes `@x` in its reply) and
//! concurrent role rendering are deferred to a follow-up PR.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as FmtWrite;
use std::io::{IsTerminal, Write as _};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::style::Stylize;
use tempfile::NamedTempFile;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::adapter::cc::CcAdapter;
use crate::adapter::codex::CodexAdapter;
use crate::adapter::gemini::GeminiAdapter;
use crate::adapter::{Engine, EngineAdapter, RoleHandle, UserMessage};
use crate::bus::MessageBus;
use crate::config::{Config, CODEROOM_DIR};
use crate::crep::{CrepEvent, StopReason};
use crate::output;
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
    /// `@all <text>` — broadcast to every running role.
    Broadcast(String),
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
    /// `/journal <role>` — ask the role to write a dated journal entry
    /// summarizing what it learned this session. Persisted at
    /// `.coderoom/journal/YYYY-MM-DD/<role>.md`; auto-loaded into the
    /// role's priors on next spawn.
    Journal(String),
    /// `/welcome` — re-show the first-run welcome card on demand, even
    /// after the `.welcomed` marker has been written.
    Welcome,
    /// `/stop <role>` — terminate the named role's subprocess.
    Stop(String),
    /// `/host <role>` — session-only host role swap.
    Host(String),
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
            "stop" if !arg.is_empty() => {
                Command::Stop(arg.strip_prefix('@').unwrap_or(arg).to_owned())
            }
            "host" if !arg.is_empty() => {
                Command::Host(arg.strip_prefix('@').unwrap_or(arg).to_owned())
            }
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
            "journal" if !arg.is_empty() => {
                let role = arg.strip_prefix('@').unwrap_or(arg).to_owned();
                if role.is_empty() {
                    Command::Help
                } else {
                    Command::Journal(role)
                }
            }
            "patch" => parse_patch_arg(arg).unwrap_or(Command::Help),
            "welcome" => Command::Welcome,
            // /help, /h, and any unknown slash command all fall through here.
            _ => Command::Help,
        };
    }
    if let Some(rest) = trimmed.strip_prefix('@') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let role = parts.next().unwrap_or("").to_owned();
        let text = parts.next().unwrap_or("").trim().to_owned();
        if role == "all" && !text.is_empty() {
            return Command::Broadcast(text);
        }
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
    stop_tx: Option<oneshot::Sender<StopReason>>,
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
///
/// If the project doesn't have a `.coderoom/` yet, this calls
/// [`crate::init::run`] first so first-time users get a working setup
/// with a single `cr start`. The auto-init message tells the user
/// where to edit the host role's priors, but the REPL proceeds anyway —
/// the default host role works out of the box for first-run dogfooding.
#[allow(clippy::too_many_lines)]
pub async fn run(project_root: &Path) -> Result<()> {
    let coderoom_dir = project_root.join(CODEROOM_DIR);
    if !coderoom_dir.exists() {
        let opts = if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            crate::init::InitOptions::manual()
        } else {
            crate::init::InitOptions::auto()
        };
        crate::init::run(project_root, opts).context("auto-initializing .coderoom/")?;
        if !coderoom_dir.exists() {
            return Ok(());
        }
        println!();
    }

    let mut cfg = Config::load(project_root)
        .with_context(|| format!("loading config in {project_root:?}"))?;
    if crate::init::offer_role_expansion(project_root, &cfg)? {
        cfg = Config::load(project_root)
            .with_context(|| format!("reloading config in {project_root:?}"))?;
        println!();
    }

    let first_run = is_first_run(&coderoom_dir);
    print_home(&cfg, &coderoom_dir, project_root, first_run);
    crate::update::maybe_notify_on_start();
    if first_run {
        mark_welcomed(&coderoom_dir).await;
    }

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

    let mut renderer_rx = bus.subscribe();
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        prompt(&mut stdout).await?;
        let line = tokio::select! {
            biased;
            signal = &mut ctrl_c => {
                signal.context("installing Ctrl-C handler")?;
                output::system("interrupt received; stopping roles...");
                shutdown_all_roles(&mut roles, StopReason::Crashed);
                anyhow::bail!("interrupted");
            }
            line = stdin.next_line() => {
                let Some(line) = line? else {
                    break;
                };
                line
            }
        };
        match parse_line(&line) {
            Command::Empty => continue,
            Command::Exit => {
                shutdown_all_roles(&mut roles, StopReason::Completed);
                break;
            }
            Command::Help => {
                print_help(&cfg);
                continue;
            }
            Command::Stop(role) => {
                if let Some(running) = roles.remove(&role) {
                    stop_running_role(&role, running, StopReason::Completed);
                    output::system(format!("stopped @{role}"));
                } else {
                    output::bad(format!("no such role: @{role}"));
                }
            }
            Command::Refresh(role) => {
                refresh_role(&cfg, &adapters, &coderoom_dir, &bus, &mut roles, &role).await;
            }
            Command::Transcript(role) => {
                show_transcript(&coderoom_dir, &role, &cfg.host_role).await;
            }
            Command::Journal(role) => {
                write_journal(
                    &mut roles,
                    &mut renderer_rx,
                    &coderoom_dir,
                    &role,
                    &cfg.host_role,
                )
                .await;
            }
            Command::Welcome => {
                print_home(&cfg, &coderoom_dir, project_root, false);
            }
            Command::Host(role) => {
                if cfg.roles.contains_key(&role) {
                    cfg.host_role.clone_from(&role);
                    output::ok(format!("@{role} is host for this session"));
                } else {
                    output::bad(format!("no such role: @{role}"));
                }
            }
            Command::Patch { role, text } => {
                if !cfg.roles.contains_key(&role) {
                    output::bad(format!("no such role: @{role}"));
                    continue;
                }
                match priors::write_patch(&coderoom_dir, &role, &text) {
                    Ok(outcome) => {
                        output::ok(format!("patched @{role} → {}", outcome.path.display()));
                        if let Some(archived) = outcome.archived {
                            output::hint(format!(
                                "(cap reached; archived oldest → {})",
                                archived.display()
                            ));
                        }
                        output::hint(
                            "applies to next /refresh; current session still uses old priors",
                        );
                    }
                    Err(error) => {
                        output::bad(format!("patch failed: {error:#}"));
                    }
                }
            }
            Command::SendTo { role, text } => {
                send_and_drain(&mut roles, &mut renderer_rx, &role, &text, &cfg.host_role).await?;
            }
            Command::Broadcast(text) => {
                let mut names: Vec<String> = roles.keys().cloned().collect();
                names.sort();
                println!(
                    "  {} {}",
                    "↳".with(output::FADE),
                    format!(
                        "broadcast → {}",
                        names
                            .iter()
                            .map(|n| format!("@{n}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                    .with(output::DIM)
                    .italic(),
                );
                for role in names {
                    send_and_drain(&mut roles, &mut renderer_rx, &role, &text, &cfg.host_role)
                        .await?;
                }
            }
            Command::SendToHost(text) => {
                let host = cfg.host_role.clone();
                send_and_drain(&mut roles, &mut renderer_rx, &host, &text, &cfg.host_role).await?;
            }
        }
    }

    shutdown_all_roles(&mut roles, StopReason::Completed);
    Ok(())
}

async fn prompt(stdout: &mut tokio::io::Stdout) -> Result<()> {
    let prompt = output::prompt();
    stdout.write_all(prompt.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

fn stop_running_role(role: &str, mut running: RunningRole, reason: StopReason) {
    drop(running.tx_user);
    if let Some(stop_tx) = running.stop_tx.take() {
        if stop_tx.send(reason).is_err() {
            debug!(role, "role subprocess stop channel already closed");
        }
    }
}

fn shutdown_all_roles(roles: &mut HashMap<String, RunningRole>, reason: StopReason) {
    let mut names: Vec<String> = roles.keys().cloned().collect();
    names.sort();
    for name in names {
        if let Some(running) = roles.remove(&name) {
            stop_running_role(&name, running, reason);
        }
    }
}

/// Marker file inside `.coderoom/` that tracks whether the first-run
/// home copy has been shown. Hidden (leading dot) so it never shows up
/// in `ls`-without-`-a`.
const WELCOMED_MARKER: &str = ".welcomed";

/// Whether to label the startup home screen as a freshly-created room
/// or a returning project.
fn is_first_run(coderoom_dir: &Path) -> bool {
    !coderoom_dir.join(WELCOMED_MARKER).exists()
}

/// Drop a marker so future `cr start` runs use returning-project copy.
/// Best-effort: a write failure here just means the user sees the
/// first-run copy twice — not worth surfacing.
async fn mark_welcomed(coderoom_dir: &Path) {
    let _ = tokio::fs::write(coderoom_dir.join(WELCOMED_MARKER), b"").await;
}

#[derive(Debug, Clone)]
struct UiCell {
    styled: String,
    visible: usize,
}

fn plain_cell(text: impl Into<String>) -> UiCell {
    let styled = text.into();
    let visible = styled.chars().count();
    UiCell { styled, visible }
}

fn styled_cell(plain: &str, styled: impl std::fmt::Display) -> UiCell {
    UiCell {
        styled: styled.to_string(),
        visible: plain.chars().count(),
    }
}

fn join_cells(parts: &[UiCell]) -> UiCell {
    let mut styled = String::new();
    let mut visible = 0;
    for part in parts {
        styled.push_str(&part.styled);
        visible += part.visible;
    }
    UiCell { styled, visible }
}

fn empty_cell() -> UiCell {
    plain_cell("")
}

// ───────────────────── boot splash ─────────────────────────
//
// The framed two-column splash printed on every `cr start` (and on the
// `/welcome` slash command). All visible columns are computed from
// `UiCell::visible`, which excludes ANSI escape bytes — that is how the
// `┌`, `│`, and `┘` glyphs stay aligned column-for-column regardless of
// how richly the inner content is styled. Right-column copy is read from
// `data/splash_content.toml` so the tips and "what's new" entries can
// move release-by-release without touching code.
//
// Width budget per row:
//   `│ ` + left(left_w) + `  ` + right(right_w) + ` │` = 6 + left_w + right_w
// We choose `width` ∈ [60, 80] from the terminal size, then split the
// inner area as left ≈ 50 % so role rows fit even at 60 cols.

const SPLASH_CONTENT_TOML: &str = include_str!("../data/splash_content.toml");

#[derive(Debug, serde::Deserialize)]
struct SplashContent {
    tips: SplashTips,
    whats_new: Vec<SplashRelease>,
}

#[derive(Debug, serde::Deserialize)]
struct SplashTips {
    items: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct SplashRelease {
    version: String,
    items: Vec<String>,
}

fn load_splash_content() -> SplashContent {
    toml::from_str(SPLASH_CONTENT_TOML)
        .expect("data/splash_content.toml must be valid TOML — checked in tests")
}

fn is_placeholder_model(model: &str) -> bool {
    let normalized = model.trim().to_ascii_lowercase();
    normalized.is_empty() || normalized == "model"
}

fn started_model_label(engine: &str, model: &str) -> String {
    if !is_placeholder_model(model) {
        return model.to_owned();
    }
    match engine {
        "cc" => "Claude default".to_owned(),
        "codex" => "Codex default".to_owned(),
        "gemini" => "Gemini default".to_owned(),
        other => format!("{other} default"),
    }
}

fn splash_engine_short(engine: Engine) -> &'static str {
    match engine {
        Engine::Cc => "cc",
        Engine::Codex => "codex",
        Engine::Gemini => "gemini",
    }
}

/// Short, fixed-width context label so role rows stay aligned even at
/// the 60-col floor. Mirrors the longer hint shown in the old dashboard
/// but trims `" context"` to keep room for the role name.
fn splash_context_short(engine: Engine, model: Option<&str>) -> &'static str {
    let normalized = model.unwrap_or_default().to_ascii_lowercase();
    match engine {
        Engine::Cc if normalized.contains("sonnet") || normalized.contains("haiku") => "200k",
        Engine::Cc => "1M",
        Engine::Codex if normalized.contains("gpt-5") => "400k",
        Engine::Gemini if normalized.contains("pro") => "1M",
        Engine::Codex | Engine::Gemini => "model",
    }
}

/// Frame piece styled with the splash teal stroke.
fn frame(s: &str) -> String {
    s.with(output::SPLASH_FRAME).to_string()
}

/// Total visible width of the splash frame.
///
/// `min(term_width − 4, 80)` clamped to a 60-col floor so the box still
/// fits on the narrowest reasonable ssh window. We subtract 4 instead of
/// 2 to leave a comfortable margin from both terminal edges.
fn splash_width() -> usize {
    let columns = crossterm::terminal::size().map_or(80, |(c, _)| usize::from(c));
    columns.saturating_sub(4).clamp(60, 80)
}

/// Split the box's inner width into a left and right column, with a
/// 2-column gap between them.
///
/// The split prefers ~40 % to the left so the right column has room for
/// tip and release-note copy without needing aggressive truncation. The
/// caller passes `role_floor` — the visible width of the widest role
/// row — and the left column never shrinks below that, so role rows
/// always fit. The right column is guaranteed at least 20 visible
/// columns; below that, headings start losing meaning.
fn splash_columns(width: usize, role_floor: usize) -> (usize, usize) {
    let inner = width.saturating_sub(4); // `│ ` and ` │`
    let gap = 2;
    let available = inner.saturating_sub(gap);
    let preferred = (available * 4) / 10;
    let max_left = available.saturating_sub(20).max(20);
    let left = preferred.max(role_floor).min(max_left);
    let right = available.saturating_sub(left);
    (left, right)
}

/// Visible width of the longest role row that will be rendered.
/// Mirrors the layout used in `splash_role_cell`:
///   `● ␠@{role:<role_pad+1}␠␠{engine:<6}␠·␠{ctx}`
fn splash_role_floor(cfg: &Config, role_names: &[&str], role_pad: usize) -> usize {
    role_names
        .iter()
        .map(|name| {
            let role_cfg = cfg.role_config(name, Path::new(""));
            let engine = role_cfg
                .as_ref()
                .map_or(cfg.default_engine, |role| role.engine);
            let model = role_cfg
                .as_ref()
                .and_then(|role| role.model.as_deref())
                .or(cfg.default_model.as_deref());
            let ctx = splash_context_short(engine, model);
            // ● + ' ' + '@' + role_pad + ' ' + ' ' + engine_pad(6) + ' ' + '·' + ' ' + ctx
            1 + 1 + 1 + role_pad + 2 + 6 + 1 + 1 + 1 + ctx.chars().count()
        })
        .max()
        .unwrap_or(28)
}

/// `┌─ {title} ─...─┐` — title is embedded into the top stroke.
///
/// Visible breakdown: `┌` + `─` + ` ` + title + ` ` + N×`─` + `┐` = width.
fn splash_top(width: usize, title: &UiCell) -> String {
    let prefix = 3; // "┌─ "
    let between = 1; // " " after title
    let suffix = 1; // "┐"
    let n = width.saturating_sub(prefix + title.visible + between + suffix);
    let mut out = String::new();
    out.push_str(&frame("┌─"));
    out.push(' ');
    out.push_str(&title.styled);
    out.push(' ');
    out.push_str(&frame(&"─".repeat(n)));
    out.push_str(&frame("┐"));
    out
}

fn splash_bottom(width: usize) -> String {
    let mut out = frame("└");
    out.push_str(&frame(&"─".repeat(width.saturating_sub(2))));
    out.push_str(&frame("┘"));
    out
}

/// Truncate a styled cell to `max_visible` columns. Falls back to the
/// plain string when truncation is needed because we can't safely cut
/// inside ANSI escape sequences.
fn fit_cell(cell: UiCell, plain: &str, max_visible: usize) -> UiCell {
    if cell.visible <= max_visible {
        cell
    } else {
        plain_cell(truncate_inline(plain, max_visible))
    }
}

/// Pad a cell to exactly `width` visible columns by appending spaces.
fn pad_cell(cell: &UiCell, width: usize) -> String {
    let pad = width.saturating_sub(cell.visible);
    let mut out = cell.styled.clone();
    for _ in 0..pad {
        out.push(' ');
    }
    out
}

/// `│ {left:left_w}  {right:right_w} │` — body row with two columns.
fn splash_pair(left: &UiCell, right: &UiCell, left_w: usize, right_w: usize) -> String {
    let mut out = frame("│");
    out.push(' ');
    out.push_str(&pad_cell(left, left_w));
    out.push_str("  ");
    out.push_str(&pad_cell(right, right_w));
    out.push(' ');
    out.push_str(&frame("│"));
    out
}

/// Read a display name for the welcome line without shelling out from
/// the async REPL runtime. The splash falls back to a nameless greeting
/// when no common user-name environment variable is set.
fn git_user_name() -> Option<String> {
    ["GIT_AUTHOR_NAME", "USER", "USERNAME"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
}

/// Display a path with `$HOME` collapsed to `~`. Falls back to the raw
/// path when home dir is unavailable or the path lives outside it.
fn home_relative_display(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            let rel_str = rel.display().to_string();
            return if rel_str.is_empty() {
                "~".to_owned()
            } else {
                format!("~/{rel_str}")
            };
        }
    }
    path.display().to_string()
}

/// Build the styled "● @role  engine · ctx" cell, padded to fit the
/// left column. The bullet and role name pick up the role's stable
/// color; engine and context render in muted neutral tones.
fn splash_role_cell(
    cfg: &Config,
    coderoom_dir: &Path,
    name: &str,
    role_pad: usize,
    max_width: usize,
) -> UiCell {
    let role_cfg = cfg.role_config(name, coderoom_dir);
    let engine = role_cfg
        .as_ref()
        .map_or(cfg.default_engine, |role| role.engine);
    let model = role_cfg
        .as_ref()
        .and_then(|role| role.model.as_deref())
        .or(cfg.default_model.as_deref());
    let engine_short = splash_engine_short(engine);
    let ctx = splash_context_short(engine, model);
    let role_paint = output::role_color(name, &cfg.host_role);
    let role_token = format!("@{name}");
    let role_padded = format!("{role_token:<width$}", width = role_pad + 1);
    let plain = format!("● {role_padded}  {engine_short:<6} · {ctx}");
    let cell = join_cells(&[
        styled_cell("●", "●".with(role_paint)),
        plain_cell(" "),
        styled_cell(&role_padded, role_padded.as_str().with(role_paint).bold()),
        plain_cell("  "),
        styled_cell(
            &format!("{engine_short:<6}"),
            format!("{engine_short:<6}").with(output::MUTE),
        ),
        styled_cell(" ", " ".with(output::FADE)),
        styled_cell("·", "·".with(output::FADE)),
        plain_cell(" "),
        styled_cell(ctx, ctx.with(output::MUTE)),
    ]);
    fit_cell(cell, &plain, max_width)
}

/// `[ 1.0k ] base tokens loaded` — the count renders inside a teal
/// pill (background fill + dark foreground). The pill text is built
/// with literal spaces around the digits so it reads as a chip rather
/// than a colored substring.
fn splash_token_pill_cell(total_tokens: u64, max_width: usize) -> UiCell {
    let formatted = priors::format_token_count(total_tokens);
    let pill_inner = format!(" {formatted} ");
    let pill_visible = pill_inner.chars().count();
    let trailer = " base tokens loaded";
    let plain = format!("{pill_inner}{trailer}");
    let cell = join_cells(&[
        styled_cell(
            &pill_inner,
            pill_inner
                .as_str()
                .with(output::SPLASH_PILL_FG)
                .on(output::SPLASH_FRAME)
                .bold(),
        ),
        styled_cell(trailer, trailer.with(output::TEXT)),
    ]);
    debug_assert_eq!(cell.visible, pill_visible + trailer.chars().count());
    fit_cell(cell, &plain, max_width)
}

/// Pick the `[[whats_new]]` entry whose `version` matches
/// `CARGO_PKG_VERSION`, falling back to the head of the list when no
/// exact match is recorded yet (e.g. a bumped Cargo.toml without a new
/// CHANGELOG entry).
fn pick_release<'a>(content: &'a SplashContent, version: &str) -> Option<&'a SplashRelease> {
    content
        .whats_new
        .iter()
        .find(|r| r.version == version)
        .or_else(|| content.whats_new.first())
}

/// Build the "● @role  engine · ctx" rows for the left column. Roles
/// are sorted alphabetically to match the rest of the dashboard.
fn splash_role_rows(
    cfg: &Config,
    coderoom_dir: &Path,
    role_names: &[&str],
    role_pad: usize,
    max_width: usize,
) -> Vec<UiCell> {
    role_names
        .iter()
        .map(|name| splash_role_cell(cfg, coderoom_dir, name, role_pad, max_width))
        .collect()
}

/// Boot splash, framed two-column edition. Shown on every `cr start`,
/// bare `cr`, and the `/welcome` slash command. The `first_run` flag
/// only swaps the greeting verb — the surrounding frame stays identical
/// so returning users see the same status surface.
fn print_home(cfg: &Config, coderoom_dir: &Path, project_root: &Path, first_run: bool) {
    let user_name = git_user_name();
    print!(
        "{}",
        render_home_at_width(
            cfg,
            coderoom_dir,
            project_root,
            first_run,
            splash_width(),
            user_name.as_deref(),
        )
    );
}

fn render_home_at_width(
    cfg: &Config,
    coderoom_dir: &Path,
    project_root: &Path,
    first_run: bool,
    width: usize,
    user_name: Option<&str>,
) -> String {
    let mut role_names: Vec<&str> = cfg.role_names().collect();
    role_names.sort_unstable();
    let total_tokens: u64 = role_names
        .iter()
        .map(|n| priors::estimate_role_tokens(coderoom_dir, n))
        .sum();
    let role_pad = role_names
        .iter()
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    let role_floor = splash_role_floor(cfg, &role_names, role_pad);
    let (left_w, right_w) = splash_columns(width, role_floor);

    // ── title (top border)
    let version_str = format!("v{}", env!("CARGO_PKG_VERSION"));
    let title = join_cells(&[
        styled_cell("codeRoom", "codeRoom".with(output::SPLASH_FRAME).bold()),
        plain_cell(" "),
        styled_cell(
            &version_str,
            version_str.as_str().with(output::SPLASH_VERSION),
        ),
    ]);

    // ── left column
    let greeting_verb = if first_run { "welcome" } else { "welcome back" };
    let greeting_cell = match user_name {
        Some(name) => {
            let head = format!("{greeting_verb}, ");
            let plain = format!("{head}{name}");
            let cell = join_cells(&[
                styled_cell(&head, head.as_str().with(output::EM).bold()),
                styled_cell(name, name.with(output::EM)),
            ]);
            fit_cell(cell, &plain, left_w)
        }
        None => fit_cell(
            styled_cell(greeting_verb, greeting_verb.with(output::EM).bold()),
            greeting_verb,
            left_w,
        ),
    };

    let path_display = home_relative_display(project_root);
    let path_cell = fit_cell(
        styled_cell(&path_display, path_display.as_str().with(output::DIM)),
        &path_display,
        left_w,
    );

    let token_cell = splash_token_pill_cell(total_tokens, left_w);

    let mut left: Vec<UiCell> = Vec::new();
    left.push(greeting_cell);
    left.push(empty_cell());
    left.extend(splash_role_rows(
        cfg,
        coderoom_dir,
        &role_names,
        role_pad,
        left_w,
    ));
    left.push(empty_cell());
    left.push(token_cell);
    left.push(path_cell);

    // ── right column
    let content = load_splash_content();
    let release = pick_release(&content, env!("CARGO_PKG_VERSION"));

    let mut right: Vec<UiCell> = Vec::new();
    let tips_heading_str = "tips for getting started";
    right.push(fit_cell(
        styled_cell(
            tips_heading_str,
            tips_heading_str.with(output::SPLASH_ACCENT).bold(),
        ),
        tips_heading_str,
        right_w,
    ));
    for tip in &content.tips.items {
        let line_plain = format!("• {tip}");
        right.push(fit_cell(
            join_cells(&[
                styled_cell("•", "•".with(output::SPLASH_ACCENT)),
                styled_cell(&format!(" {tip}"), format!(" {tip}").with(output::TEXT)),
            ]),
            &line_plain,
            right_w,
        ));
    }
    right.push(empty_cell());

    let release_version = release.map_or_else(
        || env!("CARGO_PKG_VERSION").to_owned(),
        |r| r.version.clone(),
    );
    let whats_new_heading = format!("what's new in {release_version}");
    right.push(fit_cell(
        styled_cell(
            &whats_new_heading,
            whats_new_heading
                .as_str()
                .with(output::SPLASH_ACCENT)
                .bold(),
        ),
        &whats_new_heading,
        right_w,
    ));
    if let Some(rel) = release {
        for item in &rel.items {
            let line_plain = format!("• {item}");
            right.push(fit_cell(
                join_cells(&[
                    styled_cell("•", "•".with(output::SPLASH_ACCENT)),
                    styled_cell(&format!(" {item}"), format!(" {item}").with(output::TEXT)),
                ]),
                &line_plain,
                right_w,
            ));
        }
    }
    right.push(empty_cell());
    let footer_str = "/release-notes for more";
    right.push(fit_cell(
        styled_cell(footer_str, footer_str.with(output::DIM).italic()),
        footer_str,
        right_w,
    ));

    // ── render
    let rows = left.len().max(right.len());
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", splash_top(width, &title));
    // One blank line of breathing room inside the frame top.
    let _ = writeln!(
        out,
        "{}",
        splash_pair(&empty_cell(), &empty_cell(), left_w, right_w)
    );
    for idx in 0..rows {
        let lc = left.get(idx).cloned().unwrap_or_else(empty_cell);
        let rc = right.get(idx).cloned().unwrap_or_else(empty_cell);
        let _ = writeln!(out, "{}", splash_pair(&lc, &rc, left_w, right_w));
    }
    let _ = writeln!(
        out,
        "{}",
        splash_pair(&empty_cell(), &empty_cell(), left_w, right_w)
    );
    let _ = writeln!(out, "{}", splash_bottom(width));
    let _ = writeln!(
        out,
        "  {}",
        "type a task · @role · /help · /exit"
            .with(output::DIM)
            .italic()
    );
    out
}

fn print_help(cfg: &Config) {
    println!("commands:");
    println!("  @<role> <text>      send to a specific role");
    println!("  @all <text>         broadcast to every running role");
    println!("  <text>              send to host (@{})", cfg.host_role);
    println!("  /host <role>        make role the host for this session");
    println!("  /patch <role> <…>   save a correction; loads on next /refresh");
    println!("  /refresh <role>     re-instantiate role with latest priors+patches");
    println!("  /transcript <role>  show that role's recent spoken turns");
    println!("  /journal <role>     ask role to write today's journal entry");
    println!("  /welcome            re-show the first-run welcome card");
    println!("  /stop <role>        terminate a role's subprocess");
    println!("  /help               this help");
    println!("  /exit, /quit        leave the REPL");
    println!();
    println!(
        "{}",
        "tool traces are folded live; run `cr show` for the full event log".with(output::DIM)
    );
}

/// Send `text` to `role` and drain bus events until that role finishes
/// its turn. If the role's final `RoleSpoke` mentions other running
/// roles, automatically forwards a brief to each (one hop only at v0.1
/// — multi-hop + hop-depth escalation are tracked in
/// `docs/proposed-amendments.md`).
async fn send_and_drain(
    roles: &mut HashMap<String, RunningRole>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    role: &str,
    text: &str,
    host_role: &str,
) -> Result<()> {
    let Some(captured) = drain_one_turn_with_timeout(roles, rx, role, text, host_role).await?
    else {
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
            "  {} {}",
            "↳".with(output::FADE),
            format!("auto-routing to @{mention}")
                .with(output::DIM)
                .italic(),
        );
        if drain_one_turn_with_timeout(roles, rx, mention, &brief, host_role)
            .await?
            .is_none()
        {
            break;
        }
    }
    Ok(())
}

async fn drain_one_turn_with_timeout(
    roles: &mut HashMap<String, RunningRole>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    role: &str,
    text: &str,
    host_role: &str,
) -> Result<Option<CapturedTurn>> {
    let result = tokio::time::timeout(
        PER_TURN_TIMEOUT,
        drain_one_turn(&*roles, rx, role, text, host_role),
    )
    .await;
    if let Ok(result) = result {
        result
    } else {
        output::bad(format!(
            "@{role} timed out after {}s; stopping role",
            PER_TURN_TIMEOUT.as_secs()
        ));
        if let Some(running) = roles.remove(role) {
            stop_running_role(role, running, StopReason::Crashed);
        }
        Ok(None)
    }
}

/// Frames of the standard braille spinner. ~10 frames at 100 ms gives
/// a familiar one-second rotation that matches `cargo`, `npm`, etc.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Tick interval for the spinner, in milliseconds. Below ~80 ms users
/// notice the redraws as flicker; above ~120 ms it looks frozen.
const SPINNER_TICK_MS: u64 = 100;

/// Maximum wall-clock time the REPL will wait for one role turn before
/// returning control to the user and terminating the wedged role.
const PER_TURN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Inline "@<role> thinking <spinner>" line that lives below the user's
/// last input while we wait for the role to respond. Repaints in place
/// via carriage-return + clear-line so it stays on a single screen row,
/// then erases itself before any real event is rendered.
///
/// Skips all output when stdout is not a TTY (`cr ... | tee log.txt`)
/// to keep redirected output free of ANSI escapes.
struct ThinkingSpinner {
    role: String,
    frame: usize,
    is_painted: bool,
    is_tty: bool,
}

impl ThinkingSpinner {
    fn start(role: &str) -> Self {
        let mut spinner = Self {
            role: role.to_owned(),
            frame: 0,
            is_painted: false,
            is_tty: std::io::stdout().is_terminal(),
        };
        spinner.repaint();
        spinner
    }

    fn paint(&mut self) {
        if !self.is_tty {
            return;
        }
        let frame = SPINNER_FRAMES[self.frame % SPINNER_FRAMES.len()];
        // \r returns cursor to col 0; \x1b[2K clears the whole line.
        // The role color is dropped on intentionally so the line is
        // unambiguously "status" and not confused with a RoleSpoke.
        print!(
            "\r\x1b[2K{}",
            format!("  @{} thinking {}", self.role, frame).with(output::DIM)
        );
        let _ = std::io::stdout().flush();
        self.is_painted = true;
    }

    fn advance(&mut self) {
        self.frame = (self.frame + 1) % SPINNER_FRAMES.len();
        if self.is_painted {
            self.paint();
        }
    }

    fn repaint(&mut self) {
        self.paint();
    }

    fn clear(&mut self) {
        if !self.is_tty || !self.is_painted {
            self.is_painted = false;
            return;
        }
        print!("\r\x1b[2K");
        let _ = std::io::stdout().flush();
        self.is_painted = false;
    }
}

impl Drop for ThinkingSpinner {
    fn drop(&mut self) {
        // Defensive: never leave a spinner painted on the screen if a
        // panic or early return ate the explicit clear() call.
        self.clear();
    }
}

/// Final assistant-turn fields captured during a single role's drain.
#[derive(Debug, Clone)]
struct CapturedTurn {
    text: String,
    mentions: Vec<String>,
}

/// Fold noisy tool events during a live turn. Full details are still
/// persisted in `.coderoom/messages.jsonl`; this keeps the terminal
/// focused on the user's prompt and the role's final answer.
#[derive(Debug, Default)]
struct TurnActivity {
    proposed: usize,
    completed: usize,
    failed: usize,
    tools: BTreeMap<String, usize>,
}

impl TurnActivity {
    fn from_foldable_event(event: &CrepEvent, active_role: &str) -> Option<Self> {
        match event {
            CrepEvent::ToolCallProposed {
                role, tool_name, ..
            } if role == active_role => {
                let mut tools = BTreeMap::new();
                tools.insert(tool_name.clone(), 1);
                Some(Self {
                    proposed: 1,
                    tools,
                    ..Self::default()
                })
            }
            CrepEvent::ToolCallExecuted { role, ok, .. } if role == active_role => Some(Self {
                completed: 1,
                failed: usize::from(!ok),
                ..Self::default()
            }),
            _ => None,
        }
    }

    fn merge_into(self, other: &mut Self) {
        other.proposed += self.proposed;
        other.completed += self.completed;
        other.failed += self.failed;
        for (tool, count) in self.tools {
            *other.tools.entry(tool).or_default() += count;
        }
    }

    fn summary_line(&self, role: &str) -> Option<String> {
        if self.proposed == 0 && self.completed == 0 {
            return None;
        }

        let mut parts = self
            .tools
            .iter()
            .take(4)
            .map(|(tool, count)| {
                if *count == 1 {
                    tool.clone()
                } else {
                    format!("{tool}×{count}")
                }
            })
            .collect::<Vec<_>>();
        let hidden = self.tools.len().saturating_sub(parts.len());
        if hidden > 0 {
            parts.push(format!("+{hidden}"));
        }
        let tools = if parts.is_empty() {
            "tools".to_owned()
        } else {
            parts.join(", ")
        };
        let status = if self.failed == 0 {
            format!("{} ok", self.completed)
        } else {
            format!(
                "{} ok, {} failed",
                self.completed - self.failed,
                self.failed
            )
        };
        Some(format!("  @{role} tools folded · {tools} · {status}"))
    }

    fn render_summary(&self, role: &str) {
        if let Some(line) = self.summary_line(role) {
            println!("{}", line.with(output::DIM));
        }
    }
}

/// Send `text` to `role` and drain bus events until that role's turn
/// ends. Returns the captured `RoleSpoke` info, or `None` if the role
/// stopped before producing a `RoleSpoke` (e.g., immediate crash).
///
/// Tool chatter is folded into a one-line live summary; full events are
/// still persisted in the JSONL log. This only returns to the caller
/// once the role's turn boundary is observed.
async fn drain_one_turn(
    roles: &HashMap<String, RunningRole>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    role: &str,
    text: &str,
    host_role: &str,
) -> Result<Option<CapturedTurn>> {
    let Some(running) = roles.get(role) else {
        output::bad(format!("no such role: @{role}"));
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
    let mut activity = TurnActivity::default();
    let mut spinner = ThinkingSpinner::start(role);
    let mut ticker = tokio::time::interval(Duration::from_millis(SPINNER_TICK_MS));
    // Skip the immediate fire so the spinner doesn't double-redraw on entry.
    ticker.tick().await;
    loop {
        tokio::select! {
            biased;
            recv = rx.recv() => match recv {
                Ok(event) => {
                    if let Some(hidden) = TurnActivity::from_foldable_event(&event, role) {
                        hidden.merge_into(&mut activity);
                        continue;
                    }

                    spinner.clear();
                    let done = match &event {
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
                            activity.render_summary(role);
                            render_event(&event, host_role);
                            true
                        }
                        CrepEvent::RoleStopped { role: stopped, .. } if stopped == role => {
                            activity.render_summary(role);
                            render_event(&event, host_role);
                            true
                        }
                        _ => {
                            render_event(&event, host_role);
                            false
                        }
                    };
                    if done {
                        break;
                    }
                    spinner.repaint();
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    spinner.clear();
                    output::system(format!(
                        "renderer fell behind, skipped {skipped} event(s)"
                    ));
                    spinner.repaint();
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = ticker.tick() => spinner.advance(),
        }
    }
    spinner.clear();
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
    // Loading config gives us the host role for stable lavender rendering.
    // If the config can't load (e.g. malformed), fall back to the default
    // host name — the replay still renders, lavender just won't pin.
    let host_role =
        Config::load(project_root).map_or_else(|_| "host".to_owned(), |cfg| cfg.host_role);
    let replay = MessageBus::replay(&log_path).await?;
    if replay.skipped_malformed > 0 {
        output::warn(format!(
            "{} corrupted line(s) skipped while replaying{}",
            replay.skipped_malformed,
            replay
                .first_malformed_line
                .map_or_else(String::new, |line| format!(" (first at line {line})"))
        ));
    }
    if replay.events.is_empty() {
        println!("(message log is empty)");
        return Ok(());
    }
    for event in &replay.events {
        render_event(event, &host_role);
    }
    Ok(())
}

/// Prompt the named role to write a journal entry, capture the reply,
/// and persist it under `.coderoom/journal/YYYY-MM-DD/<role>.md`.
///
/// v0.1: free-form, no schema validation. The prompt asks for cited
/// learnings explicitly so the saved markdown matches the structure
/// `priors::compose_for` expects on next spawn.
async fn write_journal(
    roles: &mut HashMap<String, RunningRole>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    coderoom_dir: &Path,
    role: &str,
    host_role: &str,
) {
    if !roles.contains_key(role) {
        output::bad(format!("no such role: @{role}"));
        return;
    }

    let prompt = "Write a journal entry summarizing what you learned in this session.\n\
        Use markdown bullets. Cite any factual claim about the codebase with either:\n\
        - a transcript anchor (`[turn 12]`-style line reference), or\n\
        - a repo file path (`src/auth/verify.go`).\n\
        Do not include claims you can't cite. Keep it under 30 lines.";

    println!(
        "{}",
        format!("asking @{role} for a journal entry...").with(output::DIM)
    );

    let Ok(Some(captured)) = drain_one_turn_with_timeout(roles, rx, role, prompt, host_role).await
    else {
        output::bad(format!("@{role} did not produce a journal entry"));
        return;
    };

    let today = chrono::Local::now().date_naive();
    let day_dir = coderoom_dir
        .join(priors::JOURNAL_DIR)
        .join(today.format("%Y-%m-%d").to_string());
    if let Err(error) = tokio::fs::create_dir_all(&day_dir).await {
        output::bad(format!("failed to create {}: {error}", day_dir.display()));
        return;
    }
    let path = day_dir.join(format!("{role}.md"));
    let body = if captured.text.ends_with('\n') {
        captured.text
    } else {
        format!("{}\n", captured.text)
    };
    if let Err(error) = tokio::fs::write(&path, body).await {
        output::bad(format!("failed to write {}: {error}", path.display()));
        return;
    }

    output::ok(format!("journal saved → {}", path.display()));
    output::hint("next spawn (or /refresh) will load this entry into the role's priors");
}

/// In-REPL: print the last few RoleSpoke events for `role` from the
/// active session's message log.
async fn show_transcript(coderoom_dir: &Path, role: &str, host_role: &str) {
    const TAIL: usize = 5;
    let log_path = coderoom_dir.join("messages.jsonl");
    if !log_path.is_file() {
        println!(
            "{}",
            "(no messages logged yet this session)".with(output::DIM)
        );
        return;
    }
    match MessageBus::replay(&log_path).await {
        Ok(replay) => {
            if replay.skipped_malformed > 0 {
                output::warn(format!(
                    "{} corrupted line(s) skipped while reading transcript",
                    replay.skipped_malformed
                ));
            }
            let filtered: Vec<&CrepEvent> = replay
                .events
                .iter()
                .filter(|e| matches!(e, CrepEvent::RoleSpoke { role: r, .. } if r == role))
                .collect();
            if filtered.is_empty() {
                println!(
                    "{}",
                    format!("(no spoken turns from @{role} yet)").with(output::DIM)
                );
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
                .with(output::DIM)
            );
            for event in &filtered[start..] {
                render_event(event, host_role);
            }
        }
        Err(error) => {
            output::bad(format!("failed to read message log: {error}"));
        }
    }
}

fn render_event(event: &CrepEvent, host_role: &str) {
    println!("{}", render_event_line(event, host_role));
    if let CrepEvent::RoleSpoke { role, cost_usd, .. } = event {
        debug!(role, cost_usd, "RoleSpoke rendered");
    }
}

fn render_event_line(event: &CrepEvent, host_role: &str) -> String {
    match event {
        CrepEvent::RoleStarted {
            role,
            engine,
            model,
            ..
        } => {
            let model = started_model_label(engine, model);
            format!(
                "{}",
                format!("[@{role} ready · model={model}]")
                    .with(output::DIM)
                    .italic()
            )
        }
        CrepEvent::RoleSpoke {
            role,
            text,
            cost_usd,
            ..
        } => {
            let _ = cost_usd;
            format!("{} {}", output::role_token(role, host_role), text)
        }
        CrepEvent::ToolCallProposed {
            role,
            tool_name,
            tool_input,
            ..
        } => {
            let summary = summarize_tool_input(tool_input);
            format!(
                "  {} @{role} · {}",
                "↳".with(output::FADE),
                format!("{tool_name} {summary}").with(output::DIM),
            )
        }
        CrepEvent::ToolCallExecuted {
            role,
            ok,
            output_summary,
            ..
        } => {
            // Tool executed lines borrow the trace shape but swap in
            // ✓/✗ in their semantic colors per docs/colors.md §4.
            let glyph = if *ok {
                "✓".with(output::OK)
            } else {
                "✗".with(output::BAD)
            };
            format!(
                "  {glyph} @{role} · {}",
                output_summary.as_str().with(output::DIM)
            )
        }
        CrepEvent::PermissionDenied {
            role,
            tool_name,
            reason,
            ..
        } => {
            // ⊘ is `warn` per the glyph table; the message tier stays dim.
            format!(
                "  {} @{role} · {}",
                "⊘".with(output::WARN),
                format!("{tool_name} denied: {reason}").with(output::DIM),
            )
        }
        CrepEvent::RoleStopped { role, reason } => {
            format!(
                "{}",
                format!("[@{role} stopped: {reason:?}]")
                    .with(output::DIM)
                    .italic()
            )
        }
    }
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
        output::bad(format!("no such role: @{role}"));
        return;
    }
    if let Some(old) = roles.remove(role) {
        stop_running_role(role, old, StopReason::Refreshed);
        // ⟳ is `warn` per docs/colors.md §4 — refresh is "attention,
        // non-fatal," not success or failure.
        println!(
            "{} {}",
            "⟳".with(output::WARN),
            format!("refreshing @{role}...").with(output::TEXT),
        );
    }
    match spawn_role(cfg, adapters, coderoom_dir, role, bus).await {
        Ok(running) => {
            roles.insert(role.to_owned(), running);
            output::ok(format!("@{role} refreshed"));
        }
        Err(error) => {
            output::bad(format!("refreshing @{role} failed: {error:#}"));
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
    let compose_dir = coderoom_dir.to_path_buf();
    let compose_role = name.to_owned();
    let composed =
        tokio::task::spawn_blocking(move || priors::compose_for(&compose_dir, &compose_role))
            .await
            .context("joining priors composer task")?
            .with_context(|| format!("composing priors for role `{name}`"))?;
    let priors_temp = writepriors_tempfile(name, &composed)
        .with_context(|| format!("staging priors for role `{name}`"))?;

    let mut role_cfg = cfg
        .role_config(name, coderoom_dir)
        .with_context(|| format!("role `{name}` is declared but has invalid config"))?;
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
        stop_tx,
    } = handle;
    spawn_event_forwarder(rname, rx_events, Arc::clone(bus));
    Ok(RunningRole {
        tx_user,
        stop_tx: Some(stop_tx),
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
    fn parse_at_all_broadcasts() {
        match parse_line("@all summarize blockers") {
            Command::Broadcast(text) => assert_eq!(text, "summarize blockers"),
            other => panic!("expected Broadcast, got {other:?}"),
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
        assert_eq!(
            parse_line("/stop @backend"),
            Command::Stop("backend".into())
        );
    }

    #[test]
    fn parse_slash_host_with_role() {
        assert_eq!(parse_line("/host backend"), Command::Host("backend".into()));
        assert_eq!(
            parse_line("/host @backend"),
            Command::Host("backend".into())
        );
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
    fn parse_journal_with_role() {
        assert_eq!(
            parse_line("/journal backend"),
            Command::Journal("backend".into())
        );
        assert_eq!(
            parse_line("/journal @backend"),
            Command::Journal("backend".into())
        );
    }

    #[test]
    fn parse_journal_without_role_shows_help() {
        assert_eq!(parse_line("/journal"), Command::Help);
    }

    #[test]
    fn parse_welcome() {
        assert_eq!(parse_line("/welcome"), Command::Welcome);
    }

    #[test]
    fn started_model_label_hides_literal_model_placeholder() {
        assert_eq!(started_model_label("codex", "model"), "Codex default");
        assert_eq!(
            started_model_label("cc", "claude-opus-4-7"),
            "claude-opus-4-7"
        );
    }

    #[tokio::test]
    async fn first_run_marker_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let coderoom = tmp.path().to_path_buf();
        // No marker yet → first run
        assert!(is_first_run(&coderoom));
        mark_welcomed(&coderoom).await;
        // Marker present → not first run
        assert!(!is_first_run(&coderoom));
        // Idempotent: second mark is a no-op (same path, same content)
        mark_welcomed(&coderoom).await;
        assert!(!is_first_run(&coderoom));
    }

    #[test]
    fn splash_content_toml_parses_and_has_required_shape() {
        let content = load_splash_content();
        assert!(
            !content.tips.items.is_empty(),
            "tips.items must list at least one entry"
        );
        assert!(
            !content.whats_new.is_empty(),
            "whats_new must list at least one release entry"
        );
        for release in &content.whats_new {
            assert!(
                !release.version.is_empty(),
                "every [[whats_new]] entry needs a version"
            );
            assert!(
                !release.items.is_empty(),
                "release {} needs at least one item",
                release.version
            );
        }
    }

    #[test]
    fn splash_pick_release_prefers_exact_version_match() {
        let content = load_splash_content();
        // Pick something we know exists in the bundled file.
        let head = content.whats_new.first().expect("at least one release");
        let picked = pick_release(&content, &head.version).expect("exact match");
        assert_eq!(picked.version, head.version);
    }

    #[test]
    fn splash_pick_release_falls_back_to_head_when_unknown() {
        let content = load_splash_content();
        let picked = pick_release(&content, "0.0.0-not-a-real-version")
            .expect("fallback to head when no match");
        assert_eq!(picked.version, content.whats_new[0].version);
    }

    #[test]
    fn splash_columns_keep_total_width_within_budget() {
        // For every reasonable width and role-floor, the row formula
        // must reproduce the requested width: 6 + left + right == width.
        for width in [60usize, 70, 80] {
            for floor in [0usize, 22, 28, 36] {
                let (left, right) = splash_columns(width, floor);
                assert_eq!(
                    6 + left + right,
                    width,
                    "row formula broke at width={width}, floor={floor}"
                );
            }
        }
    }

    fn strip_ansi(s: &str) -> String {
        // Minimal CSI stripper — enough for crossterm's SGR sequences.
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
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
    fn splash_top_and_bottom_have_exact_frame_width() {
        for width in [60usize, 70, 80] {
            let title = join_cells(&[
                styled_cell("codeRoom", "codeRoom".with(output::SPLASH_FRAME).bold()),
                plain_cell(" "),
                styled_cell("v9.9.9", "v9.9.9".with(output::SPLASH_VERSION)),
            ]);
            let top = strip_ansi(&splash_top(width, &title));
            let bottom = strip_ansi(&splash_bottom(width));
            assert_eq!(top.chars().count(), width, "top mismatched at {width}");
            assert_eq!(
                bottom.chars().count(),
                width,
                "bottom mismatched at {width}"
            );
            assert!(top.starts_with('┌'), "top must start with ┌");
            assert!(top.ends_with('┐'), "top must end with ┐");
            assert!(bottom.starts_with('└'), "bottom must start with └");
            assert!(bottom.ends_with('┘'), "bottom must end with ┘");
        }
    }

    #[test]
    fn splash_pair_rows_align_at_every_width() {
        // Mix of empty cells, narrow cells, and wide-but-fits cells.
        let cases: &[(&str, &str)] = &[
            ("", ""),
            ("welcome back, charlie", "tips for getting started"),
            ("● @host  cc · 1M", "• send a task to @host"),
            ("[ 1.0k ] base tokens", "what's new in 0.1.12"),
        ];
        for width in [60usize, 70, 80] {
            let (left_w, right_w) = splash_columns(width, 28);
            for (l, r) in cases {
                let left = plain_cell(*l);
                let right = plain_cell(*r);
                let row = strip_ansi(&splash_pair(&left, &right, left_w, right_w));
                assert_eq!(
                    row.chars().count(),
                    width,
                    "row width mismatch at {width} for ({l:?}, {r:?}): {row:?}"
                );
                assert!(row.starts_with('│'), "left edge must be │");
                assert!(row.ends_with('│'), "right edge must be │");
            }
        }
    }

    #[test]
    fn snapshot_splash_frame_shape_at_80() {
        let (left_w, right_w) = splash_columns(80, 28);
        let title = join_cells(&[
            styled_cell("codeRoom", "codeRoom".with(output::SPLASH_FRAME).bold()),
            plain_cell(" "),
            styled_cell("v0.1.12", "v0.1.12".with(output::SPLASH_VERSION)),
        ]);
        let rendered = [
            strip_ansi(&splash_top(80, &title)),
            strip_ansi(&splash_pair(
                &plain_cell("welcome back, chao"),
                &plain_cell("tips for getting started"),
                left_w,
                right_w,
            )),
            strip_ansi(&splash_bottom(80)),
        ]
        .join("\n");
        insta::assert_snapshot!(rendered, @r"
┌─ codeRoom v0.1.12 ───────────────────────────────────────────────────────────┐
│ welcome back, chao             tips for getting started                      │
└──────────────────────────────────────────────────────────────────────────────┘");
    }

    fn splash_snapshot_config() -> Config {
        Config {
            default_engine: Engine::Cc,
            default_model: None,
            budget_per_role_usd: 1.0,
            host_role: "host".into(),
            roles: HashMap::from([
                (
                    "host".into(),
                    crate::config::RoleEntry {
                        engine: Some(Engine::Cc),
                        model: Some("opus".into()),
                    },
                ),
                (
                    "backend".into(),
                    crate::config::RoleEntry {
                        engine: Some(Engine::Cc),
                        model: None,
                    },
                ),
                (
                    "security".into(),
                    crate::config::RoleEntry {
                        engine: Some(Engine::Codex),
                        model: None,
                    },
                ),
            ]),
        }
    }

    #[test]
    fn snapshot_boot_dashboard_at_80() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = splash_snapshot_config();
        let rendered = strip_ansi(&render_home_at_width(
            &cfg,
            tmp.path(),
            Path::new("/repo/codeRoom"),
            false,
            80,
            Some("Ada"),
        ))
        .trim_start_matches('\n')
        .to_owned();
        insta::assert_snapshot!(rendered, @r"
┌─ codeRoom v0.1.12 ───────────────────────────────────────────────────────────┐
│                                                                              │
│ welcome back, Ada              tips for getting started                      │
│                                • type @role to send a task to a specific ro… │
│ ● @backend   cc     · 1M       • /patch <role> persists a correction across… │
│ ● @host      cc     · 1M       • /journal <role> captures today's lessons-l… │
│ ● @security  codex  · model                                                  │
│                                what's new in 0.1.12                          │
│  0  base tokens loaded         • stop, refresh, Ctrl-C, and timeouts now te… │
│ /repo/codeRoom                 • @all, /host, cr role host, cr compact, and… │
│                                • bus replay, cost totals, and engine capabi… │
│                                                                              │
│                                /release-notes for more                       │
│                                                                              │
└──────────────────────────────────────────────────────────────────────────────┘
  type a task · @role · /help · /exit
");
    }

    #[test]
    fn snapshot_render_event_lines() {
        let events = [
            CrepEvent::RoleStarted {
                role: "backend".into(),
                engine: "cc".into(),
                model: "claude-opus-4-7".into(),
                session_id: "s".into(),
                priors_hash: "p".into(),
            },
            CrepEvent::RoleSpoke {
                role: "backend".into(),
                text: "Ready for @security.".into(),
                mentions: vec!["security".into()],
                cost_usd: 0.12,
                cache_read: 42,
            },
            CrepEvent::ToolCallProposed {
                role: "backend".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({"command": "cargo test --all-features"}),
                tool_use_id: "tool-1".into(),
            },
            CrepEvent::ToolCallExecuted {
                role: "backend".into(),
                tool_use_id: "tool-1".into(),
                ok: true,
                output_summary: "tests passed".into(),
            },
            CrepEvent::PermissionDenied {
                role: "backend".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({"command": "rm -rf target"}),
                reason: "destructive shell ops require review".into(),
            },
            CrepEvent::RoleStopped {
                role: "backend".into(),
                reason: StopReason::Refreshed,
            },
        ];
        let rendered = events
            .iter()
            .map(|event| strip_ansi(&render_event_line(event, "host")))
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(rendered, @r"
[@backend ready · model=claude-opus-4-7]
@backend Ready for @security.
  ↳ @backend · Bash `cargo test --all-features`
  ✓ @backend · tests passed
  ⊘ @backend · Bash denied: destructive shell ops require review
[@backend stopped: Refreshed]
");
    }

    #[test]
    fn turn_activity_folds_tool_events() {
        let mut activity = TurnActivity::default();
        for event in [
            CrepEvent::ToolCallProposed {
                role: "host".into(),
                tool_name: "Read".into(),
                tool_input: serde_json::json!({"file_path": "README.md"}),
                tool_use_id: "1".into(),
            },
            CrepEvent::ToolCallProposed {
                role: "host".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({"command": "ls"}),
                tool_use_id: "2".into(),
            },
            CrepEvent::ToolCallExecuted {
                role: "host".into(),
                tool_use_id: "1".into(),
                ok: true,
                output_summary: "README.md".into(),
            },
            CrepEvent::ToolCallExecuted {
                role: "host".into(),
                tool_use_id: "2".into(),
                ok: true,
                output_summary: "Cargo.toml".into(),
            },
        ] {
            TurnActivity::from_foldable_event(&event, "host")
                .expect("foldable")
                .merge_into(&mut activity);
        }

        assert_eq!(
            activity.summary_line("host").as_deref(),
            Some("  @host tools folded · Bash, Read · 2 ok")
        );
    }

    #[test]
    fn turn_activity_ignores_other_roles() {
        let event = CrepEvent::ToolCallProposed {
            role: "security".into(),
            tool_name: "Read".into(),
            tool_input: serde_json::json!({"file_path": "README.md"}),
            tool_use_id: "1".into(),
        };

        assert!(TurnActivity::from_foldable_event(&event, "host").is_none());
    }

    #[test]
    fn spinner_advances_through_all_frames() {
        // Non-TTY mode prevents writes, so we can drive `advance()` purely
        // for state-machine coverage without polluting test output.
        let mut s = ThinkingSpinner {
            role: "backend".into(),
            frame: 0,
            is_painted: false,
            is_tty: false,
        };
        for expected in 1..=SPINNER_FRAMES.len() {
            s.advance();
            assert_eq!(s.frame, expected % SPINNER_FRAMES.len());
        }
    }

    #[test]
    fn spinner_clear_is_idempotent_and_marks_unpainted() {
        let mut s = ThinkingSpinner {
            role: "backend".into(),
            frame: 0,
            is_painted: true,
            is_tty: false,
        };
        s.clear();
        assert!(!s.is_painted);
        // second clear is a no-op
        s.clear();
        assert!(!s.is_painted);
    }

    #[test]
    fn spinner_skips_paint_when_non_tty() {
        let mut s = ThinkingSpinner {
            role: "backend".into(),
            frame: 0,
            is_painted: false,
            is_tty: false,
        };
        s.repaint();
        // Non-TTY: paint should NOT mark as painted (so clear() doesn't
        // emit escape sequences into a redirected log either).
        assert!(!s.is_painted);
    }

    #[test]
    fn spinner_frames_are_all_single_glyphs() {
        // Visual stability: every frame should be exactly one display
        // column wide so the trailing `›` (or end-of-line) doesn't jump
        // between frames.
        for (i, f) in SPINNER_FRAMES.iter().enumerate() {
            assert_eq!(f.chars().count(), 1, "frame {i} ({f:?}) is not 1 char");
        }
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
