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
use std::io::{IsTerminal, Write as _};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

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
///
/// If the project doesn't have a `.coderoom/` yet, this calls
/// [`crate::init::run`] first so first-time users get a working setup
/// with a single `cr start`. The auto-init message tells the user
/// where to edit the host role's priors, but the REPL proceeds anyway —
/// the default host role works out of the box for first-run dogfooding.
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
    if first_run {
        mark_welcomed(&coderoom_dir);
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
                    &roles,
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
                send_and_drain(&roles, &mut renderer_rx, &role, &text, &cfg.host_role).await?;
            }
            Command::SendToHost(text) => {
                let host = cfg.host_role.clone();
                send_and_drain(&roles, &mut renderer_rx, &host, &text, &cfg.host_role).await?;
            }
        }
    }

    Ok(())
}

async fn prompt(stdout: &mut tokio::io::Stdout) -> Result<()> {
    let prompt = output::prompt();
    stdout.write_all(prompt.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
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
fn mark_welcomed(coderoom_dir: &Path) {
    let _ = std::fs::write(coderoom_dir.join(WELCOMED_MARKER), b"");
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

fn label_cell(label: &str, value: UiCell) -> UiCell {
    let padded = format!("{label:<8}");
    join_cells(&[
        styled_cell(&padded, padded.as_str().with(output::MUTE)),
        value,
    ])
}

fn heading_cell(text: &str) -> UiCell {
    styled_cell(text, text.with(output::EM).bold())
}

fn home_width() -> usize {
    let columns = crossterm::terminal::size().map_or(96, |(columns, _)| usize::from(columns));
    columns.saturating_sub(2).clamp(68, 118)
}

fn top_border(width: usize, title: &UiCell) -> String {
    let fill = width.saturating_sub(title.visible + 3);
    format!(
        "{}{}{}{}{}",
        border("┌"),
        border("─"),
        title.styled,
        border(&"─".repeat(fill)),
        border("┐")
    )
}

fn mid_border(left_width: usize, right_width: usize) -> String {
    section_border(left_width + right_width + 7)
}

fn section_border(width: usize) -> String {
    format!(
        "{}{}{}",
        border("├"),
        border(&"─".repeat(width.saturating_sub(2))),
        border("┤")
    )
}

fn bottom_border(width: usize) -> String {
    format!(
        "{}{}{}",
        border("└"),
        border(&"─".repeat(width.saturating_sub(2))),
        border("┘")
    )
}

fn full_line(width: usize, cell: &UiCell) -> String {
    let content_width = width.saturating_sub(4);
    format!(
        "{} {}{} {}",
        border("│"),
        cell.styled,
        " ".repeat(content_width.saturating_sub(cell.visible)),
        border("│")
    )
}

fn pair_line(left: &UiCell, right: &UiCell, left_width: usize, right_width: usize) -> String {
    format!(
        "{} {}{} {} {}{} {}",
        border("│"),
        left.styled,
        " ".repeat(left_width.saturating_sub(left.visible)),
        border("│"),
        right.styled,
        " ".repeat(right_width.saturating_sub(right.visible)),
        border("│")
    )
}

fn border(s: &str) -> String {
    s.with(output::RULE).to_string()
}

fn project_display_name(project_root: &Path) -> &str {
    project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("(this project)")
}

fn config_layers(project_root: &Path) -> String {
    let mut layers = Vec::from(["project"]);
    if crate::config_layered::user_config_path().is_some_and(|path| path.exists()) {
        layers.push("user");
    }
    if crate::config_layered::local_config_path(project_root).exists() {
        layers.push("local");
    }
    layers.join(" + ")
}

fn context_hint(engine: Engine, model: Option<&str>) -> &'static str {
    let normalized = model.unwrap_or_default().to_ascii_lowercase();
    match engine {
        Engine::Cc if normalized.contains("opus") || normalized.is_empty() => "1M context",
        Engine::Cc if normalized.contains("sonnet") || normalized.contains("haiku") => {
            "200k context"
        }
        Engine::Cc => "Claude context",
        Engine::Codex if normalized.contains("gpt-5") => "400k context",
        Engine::Gemini if normalized.contains("pro") => "1M context",
        Engine::Codex | Engine::Gemini => "model context",
    }
}

fn model_label(engine: Engine, model: Option<&str>) -> String {
    model
        .filter(|value| !is_placeholder_model(value))
        .map_or_else(
            || match engine {
                Engine::Cc => "Claude default".to_owned(),
                Engine::Codex => "Codex default".to_owned(),
                Engine::Gemini => "Gemini default".to_owned(),
            },
            ToOwned::to_owned,
        )
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

fn role_profile_cell(cfg: &Config, coderoom_dir: &Path, name: &str, max_width: usize) -> UiCell {
    let role_cfg = cfg.role_config(name, coderoom_dir);
    let engine = role_cfg
        .as_ref()
        .map_or(cfg.default_engine, |role| role.engine);
    let model = role_cfg
        .as_ref()
        .and_then(|role| role.model.as_deref())
        .or(cfg.default_model.as_deref());
    let context = context_hint(engine, model);
    let model = model_label(engine, model);
    let tokens = priors::format_token_count(priors::estimate_role_tokens(coderoom_dir, name));
    let host_suffix = if cfg.is_host(name) { "  host" } else { "" };
    let plain = format!(
        "● @{name:<13} {:<6} {:<18} {:<12} {tokens:>7} tokens{host_suffix}",
        engine.as_str(),
        model,
        context
    );
    let role_name = format!("@{name}");
    let role_paint = output::role_color(name, &cfg.host_role);
    let cell = join_cells(&[
        styled_cell("●", "●".with(role_paint)),
        plain_cell(" "),
        styled_cell(
            &format!("{role_name:<14}"),
            format!("{role_name:<14}").with(role_paint).bold(),
        ),
        plain_cell(format!(
            " {:<6} {:<18} {:<12} {tokens:>7} tokens{host_suffix}",
            engine.as_str(),
            model,
            context
        )),
    ]);
    if cell.visible <= max_width {
        cell
    } else {
        plain_cell(truncate_inline(&plain, max_width))
    }
}

/// Product-grade REPL home screen. It is shown on every `cr start` and
/// bare `cr`, not only the first time. The first-run marker only changes
/// the intro copy; the status dashboard stays useful for returning users.
fn print_home(cfg: &Config, coderoom_dir: &Path, project_root: &Path, first_run: bool) {
    let project_name = project_display_name(project_root);
    let mut role_names: Vec<&str> = cfg.role_names().collect();
    role_names.sort_unstable();
    let total_tokens: u64 = role_names
        .iter()
        .map(|n| priors::estimate_role_tokens(coderoom_dir, n))
        .sum();
    let width = home_width();
    let usable = width.saturating_sub(7);
    let left_width = (usable / 3).clamp(28, 38);
    let right_width = usable.saturating_sub(left_width);
    let content_width = width.saturating_sub(4);
    let host = format!("@{}", cfg.host_role);
    let intro = if first_run {
        "First CodeRoom launch for this project. Effective configuration loaded."
    } else {
        "Welcome back. CodeRoom loaded your effective project configuration."
    };

    let host_paint = output::role_color(&cfg.host_role, &cfg.host_role);
    let left = [
        heading_cell("Project"),
        label_cell(
            "name",
            styled_cell(
                project_name,
                truncate_inline(project_name, left_width.saturating_sub(8))
                    .with(output::EM)
                    .bold(),
            ),
        ),
        label_cell("config", plain_cell(config_layers(project_root))),
        label_cell(
            "host",
            styled_cell(&host, host.as_str().with(host_paint).bold()),
        ),
        label_cell(
            "roles",
            plain_cell(format!(
                "{} role{}",
                role_names.len(),
                if role_names.len() == 1 { "" } else { "s" }
            )),
        ),
        label_cell(
            "priors",
            plain_cell(format!(
                "{} tokens",
                priors::format_token_count(total_tokens)
            )),
        ),
    ];

    let right = [
        heading_cell("Operate"),
        join_cells(&[
            styled_cell("cr ›", "cr ›".with(output::PROMPT).bold()),
            plain_cell(" ask the host"),
        ]),
        join_cells(&[
            styled_cell("@role", "@role".with(output::KEY)),
            plain_cell(" route to a specialist"),
        ]),
        join_cells(&[
            styled_cell("/patch", "/patch".with(output::KEY)),
            plain_cell(" persist a correction"),
        ]),
        plain_cell("/help · /welcome · /exit"),
    ];

    println!();
    let title = styled_cell(
        &format!(" codeRoom v{} ", env!("CARGO_PKG_VERSION")),
        format!(" codeRoom v{} ", env!("CARGO_PKG_VERSION"))
            .with(output::EM)
            .bold(),
    );
    println!("{}", top_border(width, &title));
    println!("{}", full_line(width, &plain_cell(intro)));
    println!(
        "{}",
        full_line(
            width,
            &join_cells(&[
                plain_cell("room "),
                styled_cell(project_name, project_name.with(output::EM).bold()),
                plain_cell(" · "),
                styled_cell(&host, host.as_str().with(host_paint).bold()),
                plain_cell(format!(
                    " · {} base tokens",
                    priors::format_token_count(total_tokens)
                )),
            ]),
        )
    );
    println!("{}", mid_border(left_width, right_width));
    let rows = left.len().max(right.len());
    for idx in 0..rows {
        let left_cell = left.get(idx).cloned().unwrap_or_else(empty_cell);
        let right_cell = right.get(idx).cloned().unwrap_or_else(empty_cell);
        println!(
            "{}",
            pair_line(&left_cell, &right_cell, left_width, right_width)
        );
    }
    println!("{}", section_border(width));
    println!("{}", full_line(width, &heading_cell("Roles")));
    for name in &role_names {
        println!(
            "{}",
            full_line(
                width,
                &role_profile_cell(cfg, coderoom_dir, name, content_width)
            )
        );
    }
    println!("{}", bottom_border(width));
    println!(
        "{}",
        "type a task to begin; bare text goes to the host role".with(output::DIM)
    );
}

fn print_help(cfg: &Config) {
    println!("commands:");
    println!("  @<role> <text>      send to a specific role");
    println!("  <text>              send to host (@{})", cfg.host_role);
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
    roles: &HashMap<String, RunningRole>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    role: &str,
    text: &str,
    host_role: &str,
) -> Result<()> {
    let Some(captured) = drain_one_turn(roles, rx, role, text, host_role).await? else {
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
        if drain_one_turn(roles, rx, mention, &brief, host_role)
            .await?
            .is_none()
        {
            break;
        }
    }
    Ok(())
}

/// Frames of the standard braille spinner. ~10 frames at 100 ms gives
/// a familiar one-second rotation that matches `cargo`, `npm`, etc.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Tick interval for the spinner, in milliseconds. Below ~80 ms users
/// notice the redraws as flicker; above ~120 ms it looks frozen.
const SPINNER_TICK_MS: u64 = 100;

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
    let events = MessageBus::replay(&log_path).await?;
    if events.is_empty() {
        println!("(message log is empty)");
        return Ok(());
    }
    for event in &events {
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
    roles: &HashMap<String, RunningRole>,
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

    let Ok(Some(captured)) = drain_one_turn(roles, rx, role, prompt, host_role).await else {
        output::bad(format!("@{role} did not produce a journal entry"));
        return;
    };

    let today = chrono::Local::now().date_naive();
    let day_dir = coderoom_dir
        .join(priors::JOURNAL_DIR)
        .join(today.format("%Y-%m-%d").to_string());
    if let Err(error) = std::fs::create_dir_all(&day_dir) {
        output::bad(format!("failed to create {}: {error}", day_dir.display()));
        return;
    }
    let path = day_dir.join(format!("{role}.md"));
    let body = if captured.text.ends_with('\n') {
        captured.text
    } else {
        format!("{}\n", captured.text)
    };
    if let Err(error) = std::fs::write(&path, body) {
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
        Ok(events) => {
            let filtered: Vec<&CrepEvent> = events
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
    match event {
        CrepEvent::RoleStarted {
            role,
            engine,
            model,
            ..
        } => {
            let model = started_model_label(engine, model);
            output::system(format!("@{role} ready · model={model}"));
        }
        CrepEvent::RoleSpoke {
            role,
            text,
            cost_usd,
            ..
        } => {
            println!("{} {}", output::role_token(role, host_role), text);
            debug!(role, cost_usd, "RoleSpoke rendered");
        }
        CrepEvent::ToolCallProposed {
            role,
            tool_name,
            tool_input,
            ..
        } => {
            let summary = summarize_tool_input(tool_input);
            output::tool_trace(role, format!("{tool_name} {summary}"));
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
            println!(
                "  {glyph} @{role} · {}",
                output_summary.as_str().with(output::DIM)
            );
        }
        CrepEvent::PermissionDenied {
            role,
            tool_name,
            reason,
            ..
        } => {
            // ⊘ is `warn` per the glyph table; the message tier stays dim.
            println!(
                "  {} @{role} · {}",
                "⊘".with(output::WARN),
                format!("{tool_name} denied: {reason}").with(output::DIM),
            );
        }
        CrepEvent::RoleStopped { role, reason } => {
            output::system(format!("@{role} stopped: {reason:?}"));
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
        drop(old);
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

    #[test]
    fn first_run_marker_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let coderoom = tmp.path().to_path_buf();
        // No marker yet → first run
        assert!(is_first_run(&coderoom));
        mark_welcomed(&coderoom);
        // Marker present → not first run
        assert!(!is_first_run(&coderoom));
        // Idempotent: second mark is a no-op (same path, same content)
        mark_welcomed(&coderoom);
        assert!(!is_first_run(&coderoom));
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
