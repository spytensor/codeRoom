//! The interactive REPL.
//!
//! `cr start` enters this loop. Each user input picks exactly one role,
//! sends it a prompt, and renders bus events until that role emits a
//! `RoleSpoke`. If the role's reply contains explicit delegation lines
//! that start with `@role`, those blocks push follow-up turns onto a FIFO
//! worklist inside [`send_and_drain`], so chains like
//! `user → @host → @security → @host (synthesis)` run to completion
//! before the prompt returns. See amendment A-005 in
//! `docs/proposed-amendments.md`: the chain has no hop-depth limit;
//! it ends when the queue drains, a turn is interrupted, or the user
//! halts (`Ctrl-C` × 2 / `/halt`).
//!
//! Concurrent multi-role *parallel* rendering (a single `StatusRegion`
//! shared across simultaneously-working roles) is still tracked
//! separately as a v0.2.x deliverable.

use std::collections::{HashMap, VecDeque};
use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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
mod markdown;
mod permission_prompt;
mod render;
mod sessions;
mod show;
mod splash;
mod status;
mod text;
mod turn;
mod work;

pub use command::{parse_line, Command};
use input::InputLine;
use render::render_event;
pub use show::{show_log, ShowOptions};
use splash::{print_help, print_home};
use turn::{drain_one_turn, CapturedTurn};
use work::{render_card, TurnWork};

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
    /// Turn-level cancellation channel surfaced by the adapter.
    /// Consumed by `/halt`, `/halt @role`, and the Ctrl-C-while-turn
    /// path in `drain_one_turn_handling_ctrl_c`.
    interrupt_tx: mpsc::Sender<crate::turn::TurnId>,
    /// Composed priors temp file. Held for the role's lifetime so the
    /// path passed to the engine via `--append-system-prompt-file`
    /// remains valid until the subprocess has fully read it. Dropped
    /// at role removal, which deletes the file.
    #[allow(
        dead_code,
        reason = "kept alive only for its Drop side-effect (tempfile cleanup)"
    )]
    priors_temp: NamedTempFile,
    /// Hash of the composed priors staged for this role at spawn time.
    /// Auto-routed peer quotes include it so the receiver can audit which
    /// role identity produced the quoted payload.
    priors_hash: String,
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
    /// Start every role with a fresh engine session instead of
    /// resuming. Set by `cr start --fresh`. When true, the REPL
    /// wipes `.coderoom/sessions/ids/` before spawning any role
    /// (amendment A-006).
    pub fresh: bool,
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
        .with_context(|| format!("loading config in {}", project_root.display()))?;
    if crate::init::offer_role_expansion(project_root, &cfg)? {
        cfg = Config::load(project_root)
            .with_context(|| format!("reloading config in {}", project_root.display()))?;
        println!();
    }

    let first_run = is_first_run(&coderoom_dir);
    print_home(&cfg, &coderoom_dir, project_root, first_run);
    crate::update::maybe_notify_on_start();
    if first_run {
        mark_welcomed(&coderoom_dir).await;
    }

    // `--fresh` wipes the persisted per-role session ids so every
    // role spawns clean instead of resuming the prior conversation
    // (amendment A-006). The wipe happens AFTER splash so the user
    // can see the current role roster before sessions vanish; it
    // runs BEFORE the bus opens / roles spawn so spawn_role reads
    // a clean state.
    if options.fresh {
        if let Err(error) = sessions::clear_all(project_root) {
            output::warn(format!(
                "could not clear prior sessions for --fresh: {error}"
            ));
        } else {
            output::hint("starting fresh — prior role sessions cleared");
        }
        if let Err(error) = sessions::start_new_room_session(project_root) {
            output::warn(format!("could not create fresh room session: {error}"));
        }
    } else {
        if let Err(error) = sessions::ensure_current_room_session(project_root) {
            output::warn(format!("could not prepare room session history: {error}"));
        }
        // Surface which roles will actually resume so the user
        // isn't surprised. Stale synthetic placeholders are filtered
        // out before they reach the engine's native resume path.
        let mut resumed_wired: Vec<String> = Vec::new();
        for name in cfg.role_names() {
            let Some(session_id) = sessions::read_session_id(project_root, name).ok().flatten()
            else {
                continue;
            };
            let engine = cfg
                .roles
                .get(name)
                .and_then(|r| r.engine)
                .unwrap_or(cfg.default_engine);
            if !is_resumable_session_id(engine, name, &session_id) {
                continue;
            }
            resumed_wired.push(name.to_owned());
        }
        resumed_wired.sort();
        if !resumed_wired.is_empty() {
            let names = resumed_wired
                .iter()
                .map(|n| format!("@{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            output::hint(format!(
                "resuming prior session for {names} — pass `--fresh` to start clean"
            ));
        }
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
    let (bridge_handle, bridge_rx) = match crate::permissions::bridge::start(socket_path) {
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
    let mut live_renderer_rx = bus.subscribe_live();
    let interactive_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut stdin = if interactive_tty {
        None
    } else {
        Some(BufReader::new(tokio::io::stdin()).lines())
    };
    let mut stdout = tokio::io::stdout();
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    // Tracks the timestamp of the most recent Ctrl-C so a second
    // press within `CTRL_C_DOUBLE_PRESS_WINDOW` can be treated as
    // "exit REPL" while the first press only halts the in-flight
    // turn. Lives across drain calls so the window is honoured even
    // when the user presses Ctrl-C in two consecutive turns.
    let last_ctrl_c: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));

    // Bridge receiver is owned by the input loop while the user is at
    // the prompt and re-borrowed by `drain_one_turn` during a turn.
    // Held as Option so it can be moved into the blocking input thread
    // and returned alongside the read line.
    let mut bridge_rx_holder: Option<tokio::sync::mpsc::Receiver<BridgeRequestSink>> =
        Some(bridge_rx);

    loop {
        let input = if interactive_tty {
            // Snapshot role names per iteration so /stop and /refresh
            // additions are reflected in the next prompt's `@`-completer.
            let mut role_names: Vec<String> = roles.keys().cloned().collect();
            role_names.sort();
            let host_role = cfg.host_role.clone();
            let bridge_rx_taken = bridge_rx_holder.take();
            let (line, bridge_rx_back) =
                input::read_tty_line(role_names, bridge_rx_taken, host_role).await?;
            bridge_rx_holder = bridge_rx_back;
            line
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
                // Ctrl-C at the prompt with no turn in flight. Per
                // spec § E, first press is just a cue; second press
                // within 2s exits. The same `last_ctrl_c` timer that
                // tracks Ctrl-C-during-turn carries the state across
                // both surfaces so the user gets one uniform window
                // regardless of where they are.
                let now = std::time::Instant::now();
                let was_recent = {
                    let mut guard = last_ctrl_c.lock().expect("ctrl-c mutex poisoned");
                    let recent = guard
                        .is_some_and(|prev| now.duration_since(prev) < CTRL_C_DOUBLE_PRESS_WINDOW);
                    *guard = Some(now);
                    recent
                };
                if was_recent {
                    output::system("Ctrl-C twice; stopping all roles");
                    shutdown_all_roles(&mut roles, StopReason::Crashed);
                    anyhow::bail!("interrupted");
                }
                output::system(format!(
                    "Ctrl-C → no turn in flight. Press again within {}s to exit, or /halt to interrupt later.",
                    CTRL_C_DOUBLE_PRESS_WINDOW.as_secs()
                ));
                continue;
            }
        };
        match parse_line(&line) {
            Command::Empty => {}
            Command::Exit => {
                shutdown_all_roles(&mut roles, StopReason::Completed);
                break;
            }
            Command::Help => {
                print_help(&cfg);
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
            Command::Resume(selector) => {
                let spawn_context = SpawnContext {
                    cfg: &cfg,
                    adapters: &adapters,
                    coderoom_dir: &coderoom_dir,
                    permission_policy_path: &permission_policy_path,
                    permission_socket_path: bridge_handle.as_ref().map(BridgeHandle::socket_path),
                    permission_mode_override: options.permission_mode_override,
                    bus: &bus,
                };
                handle_resume(
                    &spawn_context,
                    &mut roles,
                    selector.as_deref(),
                    project_root,
                )
                .await;
            }
            Command::Transcript(role) => {
                show_transcript(&coderoom_dir, &role, &cfg.host_role).await;
            }
            Command::Journal(role) => {
                write_journal(
                    &mut roles,
                    &bus,
                    &mut renderer_rx,
                    &mut live_renderer_rx,
                    bridge_rx_holder.as_mut().expect("bridge_rx held by REPL"),
                    &coderoom_dir,
                    &role,
                    &cfg.host_role,
                    &last_ctrl_c,
                )
                .await;
            }
            Command::Halt(target) => handle_halt(&roles, target.as_deref()).await,
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
                    &bus,
                    &mut renderer_rx,
                    &mut live_renderer_rx,
                    bridge_rx_holder.as_mut().expect("bridge_rx held by REPL"),
                    &role,
                    &text,
                    &cfg.host_role,
                    &last_ctrl_c,
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
                        &bus,
                        &mut renderer_rx,
                        &mut live_renderer_rx,
                        bridge_rx_holder.as_mut().expect("bridge_rx held by REPL"),
                        &role,
                        &text,
                        &cfg.host_role,
                        &last_ctrl_c,
                    )
                    .await?;
                }
            }
            Command::SendToHost(text) => {
                let host = cfg.host_role.clone();
                send_and_drain(
                    &mut roles,
                    &bus,
                    &mut renderer_rx,
                    &mut live_renderer_rx,
                    bridge_rx_holder.as_mut().expect("bridge_rx held by REPL"),
                    &host,
                    &text,
                    &cfg.host_role,
                    &last_ctrl_c,
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
/// its turn. Each finished turn's explicit `@<peer>` delegation lines push
/// follow-up turns onto a FIFO worklist, so a chain like
/// `user → @host → @security → @host (synthesis)` runs to completion
/// without manual prodding from the user. Per amendment A-005
/// (`docs/proposed-amendments.md`), the chain has no depth limit; it
/// ends when the queue drains, a turn is interrupted, or the user
/// halts (`Ctrl-C` × 2 / `/halt`). Three semantic guards still apply:
///
/// 1. **Self-mention skip** — `@host` delegating to `@host` doesn't fire
///    a recursive turn; a role talking to itself is a no-op.
/// 2. **Unknown-role skip** — `@<not-running>` mentions have nobody
///    to receive the brief.
/// 3. **Grounding-gate skip** — when a turn's tool calls were
///    systematically denied, the reply is almost certainly an
///    ungrounded guess; we do NOT fan out its mentions, so the
///    hallucination stops with that turn.
#[allow(
    clippy::too_many_arguments,
    reason = "REPL command plumbing passes role maps, event drains, bridge, and Ctrl-C state"
)]
async fn send_and_drain(
    roles: &mut HashMap<String, RunningRole>,
    bus: &Arc<MessageBus>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    live_rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    bridge_rx: &mut tokio::sync::mpsc::Receiver<BridgeRequestSink>,
    role: &str,
    text: &str,
    host_role: &str,
    last_ctrl_c: &Arc<Mutex<Option<std::time::Instant>>>,
) -> Result<()> {
    // Pre-flight: validate any `@./img.png` style image refs the user
    // typed. Missing files, oversized images, or unsupported formats
    // are caught here so the user doesn't burn an API turn on a
    // request that adapters can't fulfil. Only the user-typed prompt
    // is validated; auto-routed follow-up briefs are constructed by
    // CodeRoom itself and don't carry raw image refs.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = dirs::home_dir();
    if let Err(error) = crate::image_paths::parse_image_refs(text, &cwd, home.as_deref()) {
        output::bad(format!("image: {error}"));
        return Ok(());
    }

    // Worklist of pending turns. The initial entry is the user's
    // direct dispatch; auto-routed follow-ups are appended below.
    let thread_id = crate::turn::new_thread_id();
    let mut queue: VecDeque<QueuedTurn> = VecDeque::new();
    queue.push_back(QueuedTurn {
        role: role.to_owned(),
        text: text.to_owned(),
        turn_id: crate::turn::new_turn_id(),
        thread_id,
        parent_turn_id: None,
    });

    while let Some(current) = queue.pop_front() {
        publish_turn_dispatched(bus, &current).await;
        let Some(captured) = drain_one_turn_handling_ctrl_c(
            roles,
            rx,
            live_rx,
            bridge_rx,
            &current.role,
            &current.text,
            &current.turn_id,
            &current.thread_id,
            host_role,
            last_ctrl_c,
        )
        .await?
        else {
            // Turn was interrupted or the role stopped before
            // replying. End the whole chain — the user has already
            // seen the cancellation cue and can re-issue manually
            // if they want to continue. If anything was queued
            // behind this turn, surface a hint so the user knows
            // those follow-ups did not silently fire.
            if !queue.is_empty() {
                println!(
                    "  {} {}",
                    "↳".with(output::FADE),
                    format!(
                        "{} follow-up turn(s) discarded after halt — re-issue manually if needed",
                        queue.len()
                    )
                    .with(output::DIM)
                    .italic(),
                );
            }
            return Ok(());
        };

        let known: Vec<&str> = roles.keys().map(String::as_str).collect();
        let route_instructions = extract_route_instructions(&current.role, &captured.text, &known);

        // Grounding gate: if the role's tool calls were systematically
        // denied, don't auto-route its explicit `@<peer>` delegations. The
        // reply was almost certainly an ungrounded guess (memory + git log
        // instead of source), so dispatching that guess as a brief would just
        // spread the hallucination across the team. The user still sees the
        // role's reply; they can re-issue manually after granting access.
        if captured.activity.looks_ungrounded() && !route_instructions.is_empty() {
            let activity = &captured.activity;
            let suggestion = if activity.denied > 0 {
                let names = activity.top_denied_tools(3).join(", ");
                format!(
                    " — try /allow {} or `cr start --yolo`",
                    activity
                        .top_denied_tools(1)
                        .first()
                        .cloned()
                        .unwrap_or_else(|| names.clone())
                )
            } else {
                String::new()
            };
            let summary = if activity.denied > 0 {
                format!("{} permission denial(s)", activity.denied)
            } else {
                format!("all {} tool calls failed", activity.proposed)
            };
            println!(
                "  {} {}",
                "↳".with(output::FADE),
                format!(
                    "skipping auto-route: @{} had {summary} this turn{suggestion}",
                    current.role
                )
                .with(output::DIM)
                .italic(),
            );
            continue;
        }

        // Enqueue explicit delegation blocks only. A plain prose/table
        // reference like "waiting for @backend" is not a routing command; a
        // line that starts with `@backend ...` is. Each target receives the
        // block addressed to it, not the parent's whole reply.
        let width = crossterm::terminal::size().map_or(80, |(cols, _)| usize::from(cols));
        for instruction in route_instructions {
            let priors_hash = roles
                .get(&current.role)
                .map_or("", |running| running.priors_hash.as_str());
            let brief = format_peer_brief(
                &current.role,
                priors_hash,
                &captured.turn_id,
                &instruction.brief,
            );
            // Render the Slack/Discord-style reply pointer (#99)
            // before dispatching so the user can see *which* part of
            // the parent reply triggered this hop. The handoff
            // banner from #98 then renders right beneath this when
            // the child role's turn actually starts.
            println!(
                "{}",
                render::format_reply_quote(
                    &instruction.target,
                    &current.role,
                    host_role,
                    &instruction.brief,
                    width
                )
            );
            queue.push_back(QueuedTurn {
                role: instruction.target,
                text: brief,
                turn_id: crate::turn::new_turn_id(),
                thread_id: captured.thread_id.clone(),
                parent_turn_id: Some(captured.turn_id.clone()),
            });
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct QueuedTurn {
    role: String,
    text: String,
    turn_id: crate::turn::TurnId,
    thread_id: crate::turn::TurnId,
    parent_turn_id: Option<crate::turn::TurnId>,
}

async fn publish_turn_dispatched(bus: &Arc<MessageBus>, turn: &QueuedTurn) {
    if let Err(error) = bus
        .publish(CrepEvent::TurnDispatched {
            role: turn.role.clone(),
            turn_id: turn.turn_id.clone(),
            thread_id: turn.thread_id.clone(),
            parent_turn_id: turn.parent_turn_id.clone(),
            queue_position: 0,
        })
        .await
    {
        warn!(
            role = %turn.role,
            %error,
            "failed to publish turn dispatch event"
        );
    }
}

fn format_peer_brief(sender: &str, priors_hash: &str, turn_id: &str, payload: &str) -> String {
    crate::peer_quote::format_peer_quote(sender, priors_hash, turn_id, payload)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteInstruction {
    target: String,
    brief: String,
}

/// Extract explicit delegation blocks from a role reply.
///
/// `@role` in prose is often just attribution ("@security found S1") or a
/// waiting/status note ("still waiting for @backend"). Auto-routing those
/// mentions turns reports into fresh tasks and causes cross-role churn. A
/// routable delegation must start a line (optionally after a list marker):
///
/// ```text
/// @backend review authority.py
/// 1. @qa validate these release gates
/// @frontend @security check the URL policy
/// ```
///
/// Continuation lines belong to that delegation until the next explicit
/// delegation line. This lets a host send focused multi-line briefs without
/// giving every specialist the whole team brief.
fn extract_route_instructions(
    current_role: &str,
    text: &str,
    known_roles: &[&str],
) -> Vec<RouteInstruction> {
    let mut out = Vec::new();
    let mut targets: Vec<String> = Vec::new();
    let mut block: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut block_had_blank = false;

    for line in text.lines() {
        let trimmed = line.trim_start();
        let fence_marker_line = trimmed.starts_with("```") || trimmed.starts_with("~~~");

        if !in_fence {
            if let Some((next_targets, first_line)) =
                parse_delegation_line(line, current_role, known_roles)
            {
                flush_route_block(&mut out, &mut targets, &mut block);
                block_had_blank = false;
                targets = next_targets;
                if !first_line.trim().is_empty() {
                    block.push(first_line);
                }
                continue;
            }
            if !targets.is_empty() && line_starts_with_role_mention(line) {
                flush_route_block(&mut out, &mut targets, &mut block);
                block_had_blank = false;
                continue;
            }
            if !targets.is_empty() && line_has_indented_role_mention(line) {
                flush_route_block(&mut out, &mut targets, &mut block);
                block_had_blank = false;
                continue;
            }
        }

        if !targets.is_empty() && !in_fence && block_had_blank && !is_route_continuation_line(line)
        {
            flush_route_block(&mut out, &mut targets, &mut block);
            block_had_blank = false;
        }

        if !targets.is_empty() {
            block.push(line.to_owned());
            block_had_blank = trimmed.is_empty();
        }

        if fence_marker_line {
            in_fence = !in_fence;
        }
    }

    flush_route_block(&mut out, &mut targets, &mut block);
    out
}

fn flush_route_block(
    out: &mut Vec<RouteInstruction>,
    targets: &mut Vec<String>,
    block: &mut Vec<String>,
) {
    if targets.is_empty() {
        return;
    }
    let brief = trim_blank_edges(&block.join("\n"));
    if brief.is_empty() {
        targets.clear();
    } else {
        for target in targets.drain(..) {
            out.push(RouteInstruction {
                target,
                brief: brief.clone(),
            });
        }
    }
    block.clear();
}

fn parse_delegation_line(
    line: &str,
    current_role: &str,
    known_roles: &[&str],
) -> Option<(Vec<String>, String)> {
    let mut rest = strip_leading_list_marker(line);
    let mut targets = Vec::new();

    while let Some(after_at) = rest.strip_prefix('@') {
        let (name, after_name) = take_role_name(after_at)?;
        if name == current_role || !known_roles.contains(&name.as_str()) {
            return None;
        }
        targets.push(name);
        if let Some(next_target) = next_delegation_target(after_name) {
            rest = next_target;
        } else {
            rest = trim_delegation_separator(after_name);
            break;
        }
    }

    if targets.is_empty() {
        None
    } else {
        Some((targets, rest.trim_start().to_owned()))
    }
}

fn strip_leading_list_marker(line: &str) -> &str {
    for marker in ["-", "*", "+"] {
        if let Some(rest) = line.strip_prefix(marker) {
            if rest.starts_with(char::is_whitespace) {
                return rest.trim_start();
            }
        }
    }
    if let Some(rest) = line.strip_prefix('•') {
        if rest.starts_with(char::is_whitespace) {
            return rest.trim_start();
        }
    }

    let mut digits_end = 0;
    for (idx, ch) in line.char_indices() {
        if ch.is_ascii_digit() {
            digits_end = idx + ch.len_utf8();
            continue;
        }
        break;
    }
    if digits_end > 0 {
        let rest = &line[digits_end..];
        if let Some(after_marker) = rest
            .strip_prefix('.')
            .or_else(|| rest.strip_prefix(')'))
            .filter(|s| s.starts_with(char::is_whitespace))
        {
            return after_marker.trim_start();
        }
    }

    line
}

fn is_route_continuation_line(line: &str) -> bool {
    if line.starts_with(char::is_whitespace) {
        return true;
    }
    let trimmed = line.trim_start();
    trimmed.starts_with('|')
        || trimmed.starts_with('>')
        || trimmed.starts_with("```")
        || trimmed.starts_with("~~~")
        || strip_leading_list_marker(trimmed) != trimmed
}

fn line_starts_with_role_mention(line: &str) -> bool {
    let stripped = strip_leading_list_marker(line);
    stripped
        .strip_prefix('@')
        .and_then(take_role_name)
        .is_some()
}

fn line_has_indented_role_mention(line: &str) -> bool {
    line.starts_with(char::is_whitespace) && line_starts_with_role_mention(line.trim_start())
}

fn take_role_name(input: &str) -> Option<(String, &str)> {
    let mut chars = input.char_indices();
    let (_, first) = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let mut end = first.len_utf8();
    for (idx, ch) in chars {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    Some((input[..end].to_owned(), &input[end..]))
}

fn next_delegation_target(after_name: &str) -> Option<&str> {
    let rest = trim_delegation_separator(after_name);
    if rest.starts_with('@') {
        return Some(rest);
    }
    for sep in ["and", "or"] {
        if let Some(after_sep) = rest.strip_prefix(sep) {
            if after_sep.starts_with(char::is_whitespace) {
                let after_sep = after_sep.trim_start();
                if after_sep.starts_with('@') {
                    return Some(after_sep);
                }
            }
        }
    }
    for sep in ["&", "+", "和", "与", "及", "并"] {
        if let Some(after_sep) = rest.strip_prefix(sep) {
            let after_sep = after_sep.trim_start();
            if after_sep.starts_with('@') {
                return Some(after_sep);
            }
        }
    }
    None
}

fn trim_delegation_separator(input: &str) -> &str {
    input.trim_start_matches(|ch: char| {
        ch.is_whitespace()
            || matches!(
                ch,
                ':' | '：' | ',' | '，' | '、' | '/' | '.' | '。' | ';' | '；'
            )
    })
}

fn trim_blank_edges(input: &str) -> String {
    let mut lines: Vec<&str> = input.lines().collect();
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[allow(
    clippy::too_many_arguments,
    reason = "Ctrl-C wrapper needs the same drain state plus role registry"
)]
async fn drain_one_turn_handling_ctrl_c(
    roles: &mut HashMap<String, RunningRole>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    live_rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    bridge_rx: &mut tokio::sync::mpsc::Receiver<BridgeRequestSink>,
    role: &str,
    text: &str,
    turn_id: &str,
    thread_id: &str,
    host_role: &str,
    last_ctrl_c: &Arc<Mutex<Option<std::time::Instant>>>,
) -> Result<Option<CapturedTurn>> {
    let Some((tx_user, interrupt_tx)) = roles
        .get(role)
        .map(|running| (running.tx_user.clone(), running.interrupt_tx.clone()))
    else {
        output::bad(format!("no such role: @{role}"));
        return Ok(None);
    };
    let work_state = Arc::new(Mutex::new(TurnWork::new(role, host_role, text)));

    let drain = drain_one_turn(
        tx_user,
        rx,
        live_rx,
        bridge_rx,
        role,
        text,
        turn_id,
        thread_id,
        host_role,
        Arc::clone(&work_state),
    );
    tokio::pin!(drain);

    tokio::select! {
        biased;
        signal = tokio::signal::ctrl_c() => {
            signal.context("installing Ctrl-C handler")?;
            let now = std::time::Instant::now();
            let was_recent = {
                let mut guard = last_ctrl_c.lock().expect("ctrl-c mutex poisoned");
                let recent = guard
                    .is_some_and(|prev| now.duration_since(prev) < CTRL_C_DOUBLE_PRESS_WINDOW);
                *guard = Some(now);
                recent
            };
            if was_recent {
                // Second press in window: spec § H.2 — force stop_tx
                // on every still-uninterrupted role and exit.  We don't
                // wait for in-flight cancels.
                let card = work_state
                    .lock()
                    .expect("turn work mutex poisoned")
                    .interrupted_card("Ctrl-C twice — exiting REPL");
                render_card(&card);
                output::system("Ctrl-C twice; stopping all roles");
                shutdown_all_roles(roles, StopReason::Crashed);
                anyhow::bail!("interrupted");
            }
            output::system(format!(
                "Ctrl-C → halting @{role} (press again within {}s to exit)",
                CTRL_C_DOUBLE_PRESS_WINDOW.as_secs()
            ));
            // Fire interrupt; wait for the adapter to wrap up (its
            // `TurnInterrupted` or RoleSpoke), bounded by the cancel SLO
            // from § H.1.
            let _ = interrupt_tx.send(turn_id.to_owned()).await;
            let escalate_at = tokio::time::sleep(CANCEL_SLO);
            tokio::pin!(escalate_at);
            tokio::select! {
                biased;
                result = &mut drain => result,
                () = &mut escalate_at => {
                    let card = work_state
                        .lock()
                        .expect("turn work mutex poisoned")
                        .interrupted_card(format!(
                            "halt SLO ({}s) elapsed; killing @{role}",
                            CANCEL_SLO.as_secs()
                        ));
                    render_card(&card);
                    output::bad(format!(
                        "@{role} did not respond to halt within {}s; killing role",
                        CANCEL_SLO.as_secs()
                    ));
                    if let Some(running) = roles.remove(role) {
                        stop_running_role(role, running, StopReason::Crashed);
                    }
                    Ok(None)
                }
            }
        }
        result = &mut drain => result,
    }
}

/// Window during which a second Ctrl-C press is treated as "exit
/// REPL" rather than "halt this turn." Per
/// `docs/v0.2-trust-and-interrupt.md` § E.
const CTRL_C_DOUBLE_PRESS_WINDOW: Duration = Duration::from_secs(2);

/// Handle `/halt [@role]` from the prompt. Sends a turn-cancellation
/// signal to every running role (or just one). Adapters route this
/// through their `interrupt_tx`: codex emits MCP
/// `notifications/cancelled`; gemini SIGTERMs the per-turn child;
/// cc emits a `TurnInterrupted` event for the REPL drain to honour
/// (the cc subprocess keeps running per `docs/v0.2-trust-and-interrupt.md`
/// § F.1 fallback). When invoked between turns this is a near-no-op:
/// the channel buffers the request and the next-turn drain picks it
/// up, or the message is silently dropped if no turn arrives soon.
async fn handle_halt(roles: &HashMap<String, RunningRole>, target: Option<&str>) {
    let mut targets: Vec<&String> = if let Some(name) = target {
        if !roles.contains_key(name) {
            output::bad(format!("no such role: @{name}"));
            return;
        }
        roles.keys().filter(|k| *k == name).collect()
    } else {
        roles.keys().collect()
    };
    targets.sort();
    for name in targets {
        if let Some(running) = roles.get(name) {
            if let Err(error) = running
                .interrupt_tx
                .send(crate::turn::LEGACY_TURN_ID.to_owned())
                .await
            {
                debug!(role = %name, %error, "interrupt channel closed");
            }
        }
    }
    let label = target.map_or_else(|| "all roles".to_owned(), |n| format!("@{n}"));
    output::system(format!("/halt → {label}"));
}

/// Cancel SLO — how long the REPL waits for an adapter to honour an
/// interrupt before escalating to a process kill via `stop_tx`.
/// Per `docs/v0.2-trust-and-interrupt.md` § H.1.
const CANCEL_SLO: Duration = Duration::from_secs(5);

/// Prompt the named role to write a journal entry, capture the reply,
/// and persist it under `.coderoom/journal/YYYY-MM-DD/<role>.md`.
///
/// v0.1: free-form, no schema validation. The prompt asks for cited
/// learnings explicitly so the saved markdown matches the structure
/// `priors::compose_for` expects on next spawn.
#[allow(
    clippy::too_many_arguments,
    reason = "journal command reuses the normal role drain path"
)]
async fn write_journal(
    roles: &mut HashMap<String, RunningRole>,
    bus: &Arc<MessageBus>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    live_rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    bridge_rx: &mut tokio::sync::mpsc::Receiver<BridgeRequestSink>,
    coderoom_dir: &Path,
    role: &str,
    host_role: &str,
    last_ctrl_c: &Arc<Mutex<Option<std::time::Instant>>>,
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

    let turn = QueuedTurn {
        role: role.to_owned(),
        text: prompt.to_owned(),
        turn_id: crate::turn::new_turn_id(),
        thread_id: crate::turn::new_thread_id(),
        parent_turn_id: None,
    };
    publish_turn_dispatched(bus, &turn).await;
    let Ok(Some(captured)) = drain_one_turn_handling_ctrl_c(
        roles,
        rx,
        live_rx,
        bridge_rx,
        role,
        prompt,
        &turn.turn_id,
        &turn.thread_id,
        host_role,
        last_ctrl_c,
    )
    .await
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
    // Clear the persisted resume id so the refreshed role starts a
    // fresh engine session under the reloaded priors. /refresh
    // semantically means "reload priors + start over"; carrying the
    // old session forward would leave the role talking under old
    // priors next turn. Best-effort — a missing or unreadable file
    // is fine.
    let project_root = context
        .coderoom_dir
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    if let Err(error) = sessions::clear_session_id(&project_root, role) {
        debug!(role, %error, "could not clear session id during refresh");
    }
    if let Err(error) = sessions::remove_current_room_role(&project_root, role) {
        debug!(role, %error, "could not clear role from current room session during refresh");
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

async fn handle_resume(
    context: &SpawnContext<'_>,
    roles: &mut HashMap<String, RunningRole>,
    selector: Option<&str>,
    project_root: &Path,
) {
    let session = match selector.map(str::trim).filter(|s| !s.is_empty()) {
        Some(selector) => match sessions::resolve_room_session(project_root, selector) {
            Ok(Some(session)) => session,
            Ok(None) => {
                output::bad(format!("no saved room session matches `{selector}`"));
                print_room_sessions(project_root);
                return;
            }
            Err(error) => {
                output::bad(format!("reading room sessions failed: {error}"));
                return;
            }
        },
        None => match sessions::pick_room_session(project_root) {
            Ok(Some(session)) => session,
            Ok(None) => {
                // Either no saved sessions or the user cancelled the
                // picker. Print the list as a fallback only when there
                // actually are sessions; otherwise stay quiet.
                let has_any = sessions::list_room_sessions(project_root)
                    .map(|sessions| !sessions.is_empty())
                    .unwrap_or(false);
                if has_any {
                    output::hint("resume cancelled");
                } else {
                    output::hint("no saved room sessions yet");
                }
                return;
            }
            Err(error) => {
                output::bad(format!("opening session picker failed: {error:#}"));
                return;
            }
        },
    };

    output::system(format!("resuming CodeRoom session {}", session.id));
    shutdown_all_roles(roles, StopReason::Refreshed);
    if let Err(error) = sessions::activate_room_session(project_root, &session) {
        output::bad(format!("activating session failed: {error}"));
        return;
    }

    for role in context.cfg.role_names() {
        match spawn_role(context, role).await {
            Ok(running) => {
                roles.insert(role.to_owned(), running);
            }
            Err(error) => {
                output::bad(format!("spawning @{role} from session failed: {error:#}"));
            }
        }
    }
    output::ok(format!(
        "resumed {} ({} role session{})",
        session.id,
        session.role_sessions.len(),
        if session.role_sessions.len() == 1 {
            ""
        } else {
            "s"
        }
    ));
}

fn print_room_sessions(project_root: &Path) {
    let current = sessions::read_current_room_id(project_root)
        .ok()
        .flatten()
        .unwrap_or_default();
    let sessions = match sessions::list_room_sessions(project_root) {
        Ok(sessions) => sessions,
        Err(error) => {
            output::bad(format!("reading room sessions failed: {error}"));
            return;
        }
    };
    if sessions.is_empty() {
        output::hint("no saved room sessions yet");
        return;
    }
    println!("saved CodeRoom sessions:");
    for (index, session) in sessions.iter().enumerate() {
        let role_count = session.role_sessions.len();
        let marker = if session.id == current { "*" } else { " " };
        println!(
            "  {marker} {:>2}. {}  updated {}  {} role session{}",
            index + 1,
            session.id,
            session.updated_at,
            role_count,
            if role_count == 1 { "" } else { "s" }
        );
    }
    output::hint("use `/resume <number|id|prefix|latest>` to switch");
}

/// Compose priors, stage them in a tempfile, spawn the role's
/// subprocess via the configured engine adapter, and wire its event
/// stream into `bus`. Returns the [`RunningRole`] the REPL should
/// keep alive.
/// Engine-polymorphic spawn helper used by [`spawn_role`]. Returns
/// any engine-specific error wrapped in `anyhow::Error` so the
/// caller can match on `.root_cause()` for diagnostic messages.
async fn try_start_role(
    context: &SpawnContext<'_>,
    name: &str,
    role_cfg: &crate::adapter::RoleConfig,
) -> Result<crate::adapter::RoleHandle> {
    let cfg = role_cfg.clone();
    match cfg.engine {
        Engine::Cc => context
            .adapters
            .cc
            .start(cfg)
            .await
            .map_err(anyhow::Error::from),
        Engine::Codex => context
            .adapters
            .codex
            .start(cfg)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("spawning role `{name}` (codex)")),
        Engine::Gemini => context
            .adapters
            .gemini
            .start(cfg)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("spawning role `{name}` (gemini)")),
    }
}

fn is_resumable_session_id(engine: Engine, role: &str, session_id: &str) -> bool {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Older CodeRoom builds wrote synthetic placeholders for engines
    // whose real resumable id was not known at startup. Treat those as
    // non-resumable so an upgraded build does not feed `codex-qa` or
    // `gemini-security` into the engine's native resume path.
    match engine {
        Engine::Cc => true,
        Engine::Codex => trimmed != format!("codex-{role}"),
        Engine::Gemini => trimmed != format!("gemini-{role}"),
    }
}

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
    let priors_hash = crate::adapter::cc::fingerprint(&composed);
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
    // Resume from the prior session if `.coderoom/sessions/ids/<role>.id`
    // is present (amendment A-006). If the engine rejects a stale id
    // (session was cleaned up locally, project moved disks), the first
    // spawn errors; we recover by clearing the stored id and retrying
    // once with a fresh conversation — so a broken resume never blocks
    // `cr start`.
    let project_root = coderoom_dir
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    if let Ok(Some(prior)) = sessions::read_session_id(&project_root, name) {
        if is_resumable_session_id(role_cfg.engine, name, &prior) {
            role_cfg.resume_session_id = Some(prior);
        } else if let Err(error) = sessions::clear_session_id(&project_root, name) {
            debug!(role = %name, %error, "failed to clear non-resumable legacy session id");
        }
    }

    let handle = match try_start_role(context, name, &role_cfg).await {
        Ok(handle) => handle,
        Err(err) if role_cfg.resume_session_id.is_some() => {
            // Resume attempt failed. Drop the stale id and retry
            // fresh. We print a hint so the user understands why
            // the role lost its prior conversation.
            output::warn(format!(
                "@{name} could not resume prior session ({}); falling back to a fresh conversation",
                err.root_cause()
            ));
            if let Err(clear_err) = sessions::clear_session_id(&project_root, name) {
                debug!(role = %name, %clear_err, "failed to clear stale session id");
            }
            role_cfg.resume_session_id = None;
            try_start_role(context, name, &role_cfg)
                .await
                .with_context(|| format!("spawning role `{name}` after resume fallback"))?
        }
        Err(err) => return Err(err.context(format!("spawning role `{name}`"))),
    };

    let parts = handle.into_parts();
    let crate::adapter::RoleHandleParts {
        role: rname,
        engine: _,
        tx_user,
        rx_events,
        stop_tx,
        interrupt_tx,
        tempfiles,
    } = parts;
    spawn_event_forwarder(
        rname,
        project_root.clone(),
        rx_events,
        Arc::clone(context.bus),
    );
    Ok(RunningRole {
        tx_user,
        stop_tx: Some(stop_tx),
        interrupt_tx,
        priors_temp,
        priors_hash,
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
/// Side-effect: when an event carries a resumable session id, persist it
/// into `.coderoom/sessions/ids/<role>.id` so the next `cr start` can
/// resume the conversation (amendment A-006).
fn spawn_event_forwarder(
    role: String,
    project_root: PathBuf,
    mut rx: mpsc::Receiver<CrepEvent>,
    bus: Arc<MessageBus>,
) {
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let session_id = match &event {
                CrepEvent::RoleStarted { session_id, .. }
                | CrepEvent::RoleSessionUpdated { session_id, .. } => Some(session_id),
                _ => None,
            };
            if let Some(session_id) = session_id.filter(|id| !id.trim().is_empty()) {
                if let Err(error) = sessions::write_session_id(&project_root, &role, session_id) {
                    warn!(
                        role,
                        %error,
                        "failed to persist session id for resume — next start will fresh-spawn this role"
                    );
                }
                if let Err(error) =
                    sessions::record_current_room_role_session(&project_root, &role, session_id)
                {
                    warn!(
                        role,
                        %error,
                        "failed to update room session history"
                    );
                }
            }
            if let Err(error) = bus.publish(event).await {
                warn!(role, %error, "failed to publish event to bus");
            }
        }
        debug!(role, "event forwarder exiting");
    });
}

#[cfg(test)]
mod tests;
