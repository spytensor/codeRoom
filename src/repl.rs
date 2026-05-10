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
use crate::adapter::{Engine, EngineAdapter, PermissionMode, UserMessage};
use crate::bus::MessageBus;
use crate::config::{Config, CODEROOM_DIR};
use crate::crep::{CrepEvent, StopReason};
use crate::output;
use crate::permissions::{BridgeHandle, BridgeRequestSink};
use crate::priors;

mod command;
mod input;
mod permission_prompt;
mod render;
mod show;
mod splash;
mod status;
mod text;
mod turn;

pub use command::{parse_line, Command};
use input::InputLine;
use render::render_event;
pub use show::{show_log, ShowOptions};
use splash::{print_help, print_home};
use turn::{drain_one_turn, CapturedTurn};

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
    #[allow(
        dead_code,
        reason = "kept alive only for Drop side-effects such as hook settings cleanup"
    )]
    adapter_tempfiles: Vec<NamedTempFile>,
}

struct SpawnContext<'a> {
    cfg: &'a Config,
    adapters: &'a Adapters,
    coderoom_dir: &'a Path,
    permission_policy_path: &'a Path,
    /// `Some` when the live REPL has a permission bridge listening on
    /// the given socket path; `None` for headless contexts where there
    /// is no user available to prompt.
    permission_socket_path: Option<&'a Path>,
    permission_mode_override: Option<PermissionMode>,
    bus: &'a Arc<MessageBus>,
}

/// Runtime options for `cr start`.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Session-wide permission mode override. `cr start --yolo` sets this
    /// to `Some(PermissionMode::Bypass)`.
    pub permission_mode_override: Option<PermissionMode>,
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
    run_with_options(project_root, RunOptions::default()).await
}

/// REPL entry point with explicit runtime options.
#[allow(clippy::too_many_lines)]
pub async fn run_with_options(project_root: &Path, options: RunOptions) -> Result<()> {
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

    let permission_policy_path = crate::permissions::policy_path_for_coderoom(&coderoom_dir);
    ensure_permission_policy(&permission_policy_path)?;
    if options.permission_mode_override == Some(PermissionMode::Bypass) {
        output::warn("permission_mode=bypass is active for this session");
    }

    // Permission IPC bridge — hook subprocesses connect over this Unix
    // socket to surface `ask` verdicts as real user prompts. Held for
    // the REPL's lifetime; dropped on shutdown to remove the socket.
    //
    // If the listener can't bind we fall back to a dead channel so the
    // downstream `select!` arms compile uniformly; the env var is then
    // not exported to adapters and hooks degrade to deny.
    let socket_path = coderoom_dir.join(".permission-ipc.sock");
    let (bridge_handle, mut bridge_rx) = match crate::permissions::bridge::start(socket_path) {
        Ok((handle, rx)) => (Some(handle), rx),
        Err(error) => {
            output::warn(format!(
                "permission bridge unavailable ({error}); ask-mode tool requests will deny"
            ));
            let (_dead_tx, rx) = tokio::sync::mpsc::channel::<BridgeRequestSink>(1);
            (None, rx)
        }
    };

    let mut roles: HashMap<String, RunningRole> = HashMap::new();
    for name in cfg
        .role_names()
        .map(ToOwned::to_owned)
        .collect::<Vec<String>>()
    {
        let spawn_context = SpawnContext {
            cfg: &cfg,
            adapters: &adapters,
            coderoom_dir: &coderoom_dir,
            permission_policy_path: &permission_policy_path,
            permission_socket_path: bridge_handle.as_ref().map(BridgeHandle::socket_path),
            permission_mode_override: options.permission_mode_override,
            bus: &bus,
        };
        let running = spawn_role(&spawn_context, &name).await?;
        roles.insert(name, running);
    }

    let mut renderer_rx = bus.subscribe();
    let interactive_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut stdin = if interactive_tty {
        None
    } else {
        Some(BufReader::new(tokio::io::stdin()).lines())
    };
    let mut stdout = tokio::io::stdout();
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        let input = if interactive_tty {
            // Snapshot role names per iteration so /stop and /refresh
            // additions are reflected in the next prompt's `@`-completer.
            let mut role_names: Vec<String> = roles.keys().cloned().collect();
            role_names.sort();
            input::read_tty_line(role_names).await?
        } else {
            prompt(&mut stdout).await?;
            let stdin = stdin.as_mut().expect("non-tty stdin reader");
            tokio::select! {
                biased;
                signal = &mut ctrl_c => {
                    signal.context("installing Ctrl-C handler")?;
                    InputLine::Interrupted
                }
                line = stdin.next_line() => {
                    match line? {
                        Some(line) => InputLine::Line(line),
                        None => InputLine::Eof,
                    }
                }
            }
        };
        let line = match input {
            InputLine::Line(line) => line,
            InputLine::Eof => break,
            InputLine::Interrupted => {
                output::system("interrupt received; stopping roles...");
                shutdown_all_roles(&mut roles, StopReason::Crashed);
                anyhow::bail!("interrupted");
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
                let spawn_context = SpawnContext {
                    cfg: &cfg,
                    adapters: &adapters,
                    coderoom_dir: &coderoom_dir,
                    permission_policy_path: &permission_policy_path,
                    permission_socket_path: bridge_handle.as_ref().map(BridgeHandle::socket_path),
                    permission_mode_override: options.permission_mode_override,
                    bus: &bus,
                };
                refresh_role(&spawn_context, &mut roles, &role).await;
            }
            Command::Transcript(role) => {
                show_transcript(&coderoom_dir, &role, &cfg.host_role).await;
            }
            Command::Journal(role) => {
                write_journal(
                    &mut roles,
                    &mut renderer_rx,
                    &mut bridge_rx,
                    &coderoom_dir,
                    &role,
                    &cfg.host_role,
                )
                .await;
            }
            Command::Welcome => {
                print_home(&cfg, &coderoom_dir, project_root, false);
            }
            Command::Allow(tool) => {
                update_permission_policy(&permission_policy_path, &tool, true);
            }
            Command::Deny(tool) => {
                update_permission_policy(&permission_policy_path, &tool, false);
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
                send_and_drain(
                    &mut roles,
                    &mut renderer_rx,
                    &mut bridge_rx,
                    &role,
                    &text,
                    &cfg.host_role,
                )
                .await?;
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
                    send_and_drain(
                        &mut roles,
                        &mut renderer_rx,
                        &mut bridge_rx,
                        &role,
                        &text,
                        &cfg.host_role,
                    )
                    .await?;
                }
            }
            Command::SendToHost(text) => {
                let host = cfg.host_role.clone();
                send_and_drain(
                    &mut roles,
                    &mut renderer_rx,
                    &mut bridge_rx,
                    &host,
                    &text,
                    &cfg.host_role,
                )
                .await?;
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

fn ensure_permission_policy(path: &Path) -> Result<()> {
    if !path.exists() {
        crate::permissions::PermissionPolicy::default().save(path)?;
    }
    ensure_permission_policy_gitignored(path)
}

fn ensure_permission_policy_gitignored(path: &Path) -> Result<()> {
    let Some(coderoom_dir) = path.parent() else {
        return Ok(());
    };
    let ignore_path = coderoom_dir.join(".gitignore");
    let existing = std::fs::read_to_string(&ignore_path).unwrap_or_default();
    if existing
        .lines()
        .any(|line| line.trim() == "permission_policy.json")
    {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    if updated.is_empty() {
        updated.push_str("# Runtime artifacts — never committed.\n");
    }
    updated.push_str("permission_policy.json\n");
    std::fs::write(&ignore_path, updated)
        .with_context(|| format!("writing {}", ignore_path.display()))
}

fn update_permission_policy(path: &Path, tool: &str, allow: bool) {
    let result = crate::permissions::update_policy(path, |policy| {
        if allow {
            policy.allow_tool(tool);
        } else {
            policy.deny_tool(tool);
        }
    });
    match result {
        Ok(_) if allow => output::ok(format!("{tool} allowed for this session")),
        Ok(_) => output::ok(format!("{tool} denied for this session")),
        Err(error) => output::bad(format!("updating permission policy failed: {error:#}")),
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

/// Send `text` to `role` and drain bus events until that role finishes
/// its turn. If the role's final `RoleSpoke` mentions other running
/// roles, automatically forwards a brief to each (one hop only at v0.1
/// — multi-hop + hop-depth escalation are tracked in
/// `docs/proposed-amendments.md`).
async fn send_and_drain(
    roles: &mut HashMap<String, RunningRole>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    bridge_rx: &mut tokio::sync::mpsc::Receiver<BridgeRequestSink>,
    role: &str,
    text: &str,
    host_role: &str,
) -> Result<()> {
    let Some(captured) =
        drain_one_turn_with_timeout(roles, rx, bridge_rx, role, text, host_role).await?
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
        if drain_one_turn_with_timeout(roles, rx, bridge_rx, mention, &brief, host_role)
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
    bridge_rx: &mut tokio::sync::mpsc::Receiver<BridgeRequestSink>,
    role: &str,
    text: &str,
    host_role: &str,
) -> Result<Option<CapturedTurn>> {
    let Some(tx_user) = roles.get(role).map(|running| running.tx_user.clone()) else {
        output::bad(format!("no such role: @{role}"));
        return Ok(None);
    };

    let result = tokio::select! {
        biased;
        signal = tokio::signal::ctrl_c() => {
            signal.context("installing Ctrl-C handler")?;
            output::system("interrupt received; stopping roles...");
            shutdown_all_roles(roles, StopReason::Crashed);
            anyhow::bail!("interrupted");
        }
        result = tokio::time::timeout(
        PER_TURN_TIMEOUT,
            drain_one_turn(tx_user, rx, bridge_rx, role, text, host_role),
        ) => result,
    };
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

/// Maximum wall-clock time the REPL will wait for one role turn before
/// returning control to the user and terminating the wedged role.
const PER_TURN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Prompt the named role to write a journal entry, capture the reply,
/// and persist it under `.coderoom/journal/YYYY-MM-DD/<role>.md`.
///
/// v0.1: free-form, no schema validation. The prompt asks for cited
/// learnings explicitly so the saved markdown matches the structure
/// `priors::compose_for` expects on next spawn.
async fn write_journal(
    roles: &mut HashMap<String, RunningRole>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    bridge_rx: &mut tokio::sync::mpsc::Receiver<BridgeRequestSink>,
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

    let Ok(Some(captured)) =
        drain_one_turn_with_timeout(roles, rx, bridge_rx, role, prompt, host_role).await
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

/// Drop the running role and re-spawn it with the freshly composed
/// priors. Validates that the role exists in the loaded config; prints
/// status to stdout via the same coloured channel as the rest of the
/// REPL.
async fn refresh_role(
    context: &SpawnContext<'_>,
    roles: &mut HashMap<String, RunningRole>,
    role: &str,
) {
    if !context.cfg.roles.contains_key(role) {
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
    match spawn_role(context, role).await {
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
async fn spawn_role(context: &SpawnContext<'_>, name: &str) -> Result<RunningRole> {
    let cfg = context.cfg;
    let coderoom_dir = context.coderoom_dir;
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
    role_cfg.permission_policy_path = Some(context.permission_policy_path.to_path_buf());
    role_cfg.permission_socket_path = context.permission_socket_path.map(Path::to_path_buf);
    if let Some(mode) = context.permission_mode_override {
        role_cfg.permission_mode = mode;
    }

    let handle = match role_cfg.engine {
        Engine::Cc => context
            .adapters
            .cc
            .start(role_cfg)
            .await
            .with_context(|| format!("spawning role `{name}`"))?,
        Engine::Codex => context
            .adapters
            .codex
            .start(role_cfg)
            .await
            .with_context(|| format!("spawning role `{name}` (codex)"))?,
        Engine::Gemini => context
            .adapters
            .gemini
            .start(role_cfg)
            .await
            .with_context(|| format!("spawning role `{name}` (gemini)"))?,
    };

    let parts = handle.into_parts();
    let crate::adapter::RoleHandleParts {
        role: rname,
        engine: _,
        tx_user,
        rx_events,
        stop_tx,
        tempfiles,
    } = parts;
    spawn_event_forwarder(rname, rx_events, Arc::clone(context.bus));
    Ok(RunningRole {
        tx_user,
        stop_tx: Some(stop_tx),
        priors_temp,
        adapter_tempfiles: tempfiles,
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
mod tests;
