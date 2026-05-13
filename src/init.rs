//! `cr init` — bootstrap a project's `.coderoom/` directory.
//!
//! Default behaviour: scan the project (filename-only, **no network**),
//! print a transparent summary of what was found and what will be
//! written, ask `proceed? [Y/n]`, then generate the file tree.
//!
//! Non-interactive mode (`cr init -y`) skips the prompts and accepts
//! every default. `cr start` calls `run(.., InitOptions::auto())` when
//! `.coderoom/` is missing, so first-time users only need one command.
//!
//! Idempotent-by-refusing-to-overwrite: an existing `.coderoom/` errors
//! with a clear message rather than silently merging.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};

use anyhow::{bail, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Color, Stylize};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::{Handle as SignalHandle, Signals};

use crate::adapter::Engine;
use crate::config::{Config, CODEROOM_DIR, CONFIG_FILE, ROLES_DIR};
use crate::detect;
use crate::output;
use crate::role::{self, RoleAddition};

mod labels;
mod render;
mod write;

use labels::{
    engine_color, engine_install_hint, engine_label, engine_note, human_label, model_label,
    project_name, role_color, role_info, InstalledEngines,
};
#[cfg(test)]
use render::picker_row;
use render::{
    render_confirm, render_engine_picker, render_role_expansion_picker, render_role_picker,
};
use write::write_all;

/// The host-role name baked into the wizard. `init` runs before
/// `.coderoom/config.toml` is committed, so the host name is fixed at
/// "host" until the first `Config::load` after the wizard finishes.
const WIZARD_HOST_ROLE: &str = "host";

const DEFAULT_HOST_PRIORS: &str = include_str!("init_defaults/host.md");
const DEFAULT_SHARED_PRIORS: &str = include_str!("init_defaults/shared.md");
const DEFAULT_GITIGNORE: &str = include_str!("init_defaults/gitignore");
const DEFAULT_ROLE_TEMPLATE: &str = include_str!("init_defaults/role_template.md");
const ROLE_SUGGESTIONS_DISMISSED: &str = "sessions/role-suggestions-dismissed";

const DEFAULT_ENGINE: Engine = Engine::Cc;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RolePlan {
    name: String,
    engine: Engine,
}

#[derive(Debug, Clone, Copy)]
struct RoleInfo {
    name: &'static str,
    description: &'static str,
}

const ROLE_CATALOG: &[RoleInfo] = &[
    RoleInfo {
        name: "host",
        description: "orchestrates requests and keeps the room coherent",
    },
    RoleInfo {
        name: "backend",
        description: "APIs, services, storage boundaries",
    },
    RoleInfo {
        name: "frontend",
        description: "UI, components, routing, client-side state",
    },
    RoleInfo {
        name: "security",
        description: "authn, authz, threat modeling",
    },
    RoleInfo {
        name: "data",
        description: "schemas, migrations, query patterns",
    },
    RoleInfo {
        name: "devops",
        description: "CI/CD, infra, deploys, runtime health",
    },
    RoleInfo {
        name: "ci",
        description: "workflows, checks, release gates",
    },
    RoleInfo {
        name: "qa",
        description: "test strategy, edge cases, regression risk",
    },
    RoleInfo {
        name: "docs",
        description: "technical writing, examples, API reference",
    },
];

/// Knobs for [`run`].
#[derive(Debug, Clone, Copy)]
pub struct InitOptions {
    /// Skip every interactive prompt and accept defaults. Used by
    /// `cr init -y` and by `cr start`'s auto-init path.
    pub yes: bool,
    /// When `true`, the function prints a brief auto-init notice
    /// instead of the full transparent summary. Used by `cr start`.
    pub quiet_intro: bool,
}

impl InitOptions {
    /// Default for explicit `cr init` invocations: prompt before
    /// writing, full summary.
    #[must_use]
    pub const fn manual() -> Self {
        Self {
            yes: false,
            quiet_intro: false,
        }
    }

    /// Explicit `cr init -y`: skip prompts, but still show the same
    /// transparent summary as manual init.
    #[must_use]
    pub const fn accepted_defaults() -> Self {
        Self {
            yes: true,
            quiet_intro: false,
        }
    }

    /// Default for `cr start`'s auto-init path: no prompts, brief
    /// notice.
    #[must_use]
    pub const fn auto() -> Self {
        Self {
            yes: true,
            quiet_intro: true,
        }
    }
}

/// Initialize a project's `.coderoom/` directory at `project_root`.
pub fn run(project_root: &Path, options: InitOptions) -> Result<()> {
    let coderoom_dir = project_root.join(CODEROOM_DIR);
    if coderoom_dir.exists() {
        bail!(
            "{} already exists — refusing to overwrite. \
             edit it directly, or `rm -rf {}` to start over.",
            coderoom_dir.display(),
            coderoom_dir.display(),
        );
    }

    let scan = detect::scan(project_root);
    let installed = detect_installed_engines();
    let default_plan = default_role_plan(&scan.suggested_roles, &installed);

    let role_plan = if options.quiet_intro {
        print_auto_intro(&scan);
        default_plan
    } else if options.yes {
        print_full_summary(project_root, &scan, &installed, &default_plan);
        default_plan
    } else if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        if let Some(plan) = run_wizard(project_root, &scan, &installed, default_plan)? {
            plan
        } else {
            println!("aborted. nothing was written.");
            return Ok(());
        }
    } else {
        print_full_summary(project_root, &scan, &installed, &default_plan);
        if !confirm_proceed()? {
            println!("aborted. nothing was written.");
            return Ok(());
        }
        default_plan
    };

    write_all(&coderoom_dir, &role_plan)?;

    println!();
    println!(
        "{} {}",
        "✓ wrote".green().bold(),
        coderoom_dir.display().to_string().bold()
    );
    println!(
        "  {}",
        "next: cr start   ·   edit .coderoom/roles/<role>.md when you want deeper priors".dim()
    );
    Ok(())
}

/// Offer to expand an older/minimal `.coderoom/` that contains only
/// the default `@host` role. Returns `true` when config changed and
/// the caller should reload [`Config`] before spawning roles.
pub fn offer_role_expansion(project_root: &Path, cfg: &Config) -> Result<bool> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(false);
    }
    if !is_default_host_only(cfg) || role_suggestions_dismissed(project_root) {
        return Ok(false);
    }

    let scan = detect::scan(project_root);
    if !scan.suggested_roles.iter().any(|role| *role != "host") {
        return Ok(false);
    }

    match prompt_role_expansion(&scan)? {
        ExpansionPrompt::Add => {}
        ExpansionPrompt::Later => return Ok(false),
        ExpansionPrompt::Never => {
            mark_role_suggestions_dismissed(project_root);
            return Ok(false);
        }
    }

    let installed = detect_installed_engines();
    let default_plan = expansion_default_plan(&scan, cfg, &installed);
    let Some(plan) =
        run_role_expansion_picker(project_root, &scan, cfg, &installed, &default_plan)?
    else {
        return Ok(false);
    };

    let additions = role_additions_from_plan(&plan, cfg);
    if additions.is_empty() {
        println!("{}", "no new roles selected.".dark_grey());
        mark_role_suggestions_dismissed(project_root);
        return Ok(false);
    }

    let added = role::add_many(project_root, &additions)?;
    if added == 0 {
        return Ok(false);
    }

    let names = additions
        .iter()
        .map(|addition| format!("@{}", addition.name))
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "{} {}",
        format!("✓ added {added} role{}", if added == 1 { "" } else { "s" })
            .green()
            .bold(),
        names.dark_grey()
    );
    println!(
        "  {}",
        "review .coderoom/roles/<role>.md when you want deeper project priors".dark_grey()
    );
    Ok(true)
}

fn is_default_host_only(cfg: &Config) -> bool {
    cfg.host_role == "host" && cfg.roles.len() == 1 && cfg.roles.contains_key("host")
}

fn role_suggestions_dismissed(project_root: &Path) -> bool {
    project_root
        .join(CODEROOM_DIR)
        .join(ROLE_SUGGESTIONS_DISMISSED)
        .exists()
}

fn mark_role_suggestions_dismissed(project_root: &Path) {
    let marker = project_root
        .join(CODEROOM_DIR)
        .join(ROLE_SUGGESTIONS_DISMISSED);
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(marker, b"");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpansionPrompt {
    Add,
    Later,
    Never,
}

fn prompt_role_expansion(scan: &detect::ProjectScan) -> Result<ExpansionPrompt> {
    let suggested = scan
        .suggested_roles
        .iter()
        .filter(|role| **role != "host")
        .map(|role| format!("@{role}"))
        .collect::<Vec<_>>()
        .join(", ");

    println!();
    println!("{} {}", "coderoom".bold(), "· role suggestions".dark_grey());
    println!(
        "  {} {}",
        "only @host is configured; local scan suggests".dark_grey(),
        suggested.with(Color::White)
    );
    println!("  {}", "add suggested roles now? [Y/skip/no]".dark_grey());

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    loop {
        print!("roles? [Y/skip/no]  ");
        stdout.flush().ok();
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let answer = line.trim().to_ascii_lowercase();
        match answer.as_str() {
            "" | "y" | "yes" => return Ok(ExpansionPrompt::Add),
            "s" | "skip" | "later" => return Ok(ExpansionPrompt::Later),
            "n" | "no" | "never" | "d" | "dismiss" => return Ok(ExpansionPrompt::Never),
            _ => println!("(answer y, skip, or no)"),
        }
    }
}

fn expansion_default_plan(
    scan: &detect::ProjectScan,
    cfg: &Config,
    installed: &InstalledEngines,
) -> Vec<RolePlan> {
    scan.suggested_roles
        .iter()
        .map(|name| RolePlan {
            name: (*name).to_owned(),
            engine: expansion_engine_for_role(name, cfg, installed),
        })
        .collect()
}

fn expansion_engine_for_role(role: &str, cfg: &Config, installed: &InstalledEngines) -> Engine {
    if cfg.default_model.is_none() && matches!(role, "security" | "qa") && installed.codex {
        Engine::Codex
    } else {
        cfg.default_engine
    }
}

fn run_role_expansion_picker(
    project_root: &Path,
    scan: &detect::ProjectScan,
    cfg: &Config,
    installed: &InstalledEngines,
    default_plan: &[RolePlan],
) -> Result<Option<Vec<RolePlan>>> {
    let mut terminal = WizardTerminal::enter()?;
    let mut choices = build_role_choices(default_plan);
    let mut cursor = choices
        .iter()
        .position(|choice| choice.info.name != "host" && choice.selected)
        .unwrap_or(0);

    loop {
        terminal.render(&render_role_expansion_picker(
            project_root,
            scan,
            &choices,
            cursor,
        ))?;
        match read_key()? {
            WizardKey::Abort | WizardKey::Back => return Ok(None),
            WizardKey::Left | WizardKey::Right => {}
            WizardKey::Up => cursor = cursor.saturating_sub(1),
            WizardKey::Down => {
                if cursor + 1 < choices.len() {
                    cursor += 1;
                }
            }
            WizardKey::Toggle => {
                if choices[cursor].info.name != "host" {
                    choices[cursor].selected = !choices[cursor].selected;
                }
            }
            WizardKey::Enter => break,
        }
    }

    let roles = selected_role_names(&choices);
    Ok(Some(
        roles
            .iter()
            .map(|name| RolePlan {
                name: name.clone(),
                engine: expansion_engine_for_role(name, cfg, installed),
            })
            .collect(),
    ))
}

fn role_additions_from_plan(plan: &[RolePlan], cfg: &Config) -> Vec<RoleAddition> {
    plan.iter()
        .filter(|role| role.name != cfg.host_role && !cfg.roles.contains_key(&role.name))
        .map(|role| RoleAddition {
            name: role.name.clone(),
            engine: if cfg.default_model.is_some() {
                None
            } else {
                (role.engine != cfg.default_engine).then_some(role.engine)
            },
            model: None,
        })
        .collect()
}

/// What `cr init` will create on disk, in render order.
fn planned_files(coderoom_dir: &Path, roles: &[RolePlan]) -> Vec<PathBuf> {
    let mut paths = vec![
        coderoom_dir.join(CONFIG_FILE),
        coderoom_dir.join("shared.md"),
    ];
    let roles_dir = coderoom_dir.join(ROLES_DIR);
    for role in roles {
        paths.push(roles_dir.join(format!("{}.md", role.name)));
    }
    paths.push(coderoom_dir.join(".gitignore"));
    paths
}

fn default_role_plan(suggested_roles: &[&str], installed: &InstalledEngines) -> Vec<RolePlan> {
    let mut names = suggested_roles.to_vec();
    if names.is_empty() {
        names.push("host");
    }

    names
        .into_iter()
        .map(|name| RolePlan {
            name: name.to_owned(),
            engine: default_engine_for_role(name, installed),
        })
        .collect()
}

fn default_engine_for_role(role: &str, installed: &InstalledEngines) -> Engine {
    if matches!(role, "security" | "qa") && installed.codex {
        return Engine::Codex;
    }
    preferred_engine(installed)
}

fn preferred_engine(installed: &InstalledEngines) -> Engine {
    if installed.cc {
        Engine::Cc
    } else if installed.codex {
        Engine::Codex
    } else if installed.gemini {
        Engine::Gemini
    } else {
        DEFAULT_ENGINE
    }
}

/// Brief notice used when `cr start` auto-inits — the user didn't ask
/// for a wall of text, just a heads-up that we're setting things up.
fn print_auto_intro(scan: &detect::ProjectScan) {
    let role_list = scan.suggested_roles.join(", @");
    println!("{} {}", "coderoom".bold(), "· first run setup".dark_grey());
    println!(
        "  {} @{} {}",
        "no .coderoom/ found; bootstrapping".dark_grey(),
        role_list,
        "(engine defaults chosen automatically)".dark_grey()
    );
    println!(
        "  {}",
        "edit .coderoom/roles/<role>.md later to give each role real priors.".dark_grey()
    );
    println!();
}

/// Full transparent summary for explicit `cr init`.
fn print_full_summary(
    project_root: &Path,
    scan: &detect::ProjectScan,
    installed: &InstalledEngines,
    roles: &[RolePlan],
) {
    let project_name = project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("(this project)");
    let to_write = planned_files(&project_root.join(CODEROOM_DIR), roles);

    println!();
    println!(
        "{} {} {}",
        "cr init".bold(),
        "·".dark_grey(),
        format!("setting up coderoom in {project_name}").dark_grey()
    );
    println!();

    println!(
        "{} {}",
        "detect project".bold(),
        "· local scan, no network".dark_grey()
    );
    if scan.stack.is_empty() {
        println!(
            "  {} no recognised stack signals at the project root",
            "·".dark_grey()
        );
    } else {
        for signal in &scan.stack {
            println!(
                "  {} {}",
                "✓".green(),
                human_label(signal).with(Color::White)
            );
        }
    }
    println!();

    print_engine_summary(installed);
    println!();

    print_role_summary(roles);
    println!();

    println!(
        "{} {}",
        "ready to write".bold(),
        "· nothing on disk yet".dark_grey()
    );
    println!();
    println!("  {}/", project_root.join(CODEROOM_DIR).display());
    for path in to_write {
        if let Ok(rel) = path.strip_prefix(project_root.join(CODEROOM_DIR)) {
            println!("  {}", format_tree_path(rel).dark_grey());
        }
    }
    println!();

    if let Some(line_count) = scan.existing_claude_md() {
        println!(
            "  {} found existing {} ({} lines). split assistance is not automated yet.",
            "!".yellow(),
            "CLAUDE.md".bold(),
            line_count
        );
        println!("    {}", "coderoom will leave it untouched.".dark_grey());
        println!();
    }
}

fn print_engine_summary(installed: &InstalledEngines) {
    println!(
        "{} {}",
        "detect engines".bold(),
        "· each role can use a different CLI".with(output::DIM)
    );
    for engine in [Engine::Cc, Engine::Codex, Engine::Gemini] {
        // Pad PLAIN, style after — `{:<13}` on a `StyledContent` would
        // count the SGR escapes in the padding budget and break alignment.
        let label_padded = format!("{:<13}", engine_label(engine));
        if installed.is_present(engine) {
            println!(
                "  {} {} {}",
                "✓".with(output::OK),
                label_padded.with(output::EM),
                engine_install_hint(engine).with(output::DIM),
            );
        } else {
            println!(
                "  {} {} {} {}",
                "✗".with(output::BAD),
                label_padded.with(output::DIM),
                "not installed ·".with(output::DIM),
                engine_install_hint(engine).with(output::WARN),
            );
        }
    }
}

fn print_role_summary(roles: &[RolePlan]) {
    println!(
        "{} {}",
        "assign roles".bold(),
        "· generated from detected project signals".with(output::DIM)
    );
    let header = format!("  {:<13} {:<13} {}", "role", "engine", "focus");
    println!("{}", header.with(output::DIM));
    for role in roles {
        let info = role_info(&role.name);
        // Pad PLAIN before applying role / engine colors.
        let role_token = format!("@{:<width$}", role.name, width = 12);
        let engine_label_padded = format!("{:<13}", engine_label(role.engine));
        println!(
            "  {} {} {}",
            role_token.with(role_color(&role.name)),
            engine_label_padded.with(engine_color(role.engine)),
            info.description.with(output::DIM),
        );
    }
}

fn format_tree_path(path: &Path) -> String {
    format!("├─ {}", path.display())
}

/// `[Y/n]` prompt. Returns `true` for accept (default), `false` for
/// decline. Any other input re-prompts.
fn confirm_proceed() -> Result<bool> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    loop {
        print!("proceed? [Y/n]  ");
        stdout.flush().ok();
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let answer = line.trim().to_ascii_lowercase();
        match answer.as_str() {
            "" | "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("(answer y or n)"),
        }
    }
}

fn run_wizard(
    project_root: &Path,
    scan: &detect::ProjectScan,
    installed: &InstalledEngines,
    default_plan: Vec<RolePlan>,
) -> Result<Option<Vec<RolePlan>>> {
    let mut terminal = WizardTerminal::enter()?;
    let mut choices = build_role_choices(&default_plan);
    let mut cursor = 0usize;

    loop {
        terminal.render(&render_role_picker(project_root, scan, &choices, cursor))?;
        match read_key()? {
            WizardKey::Abort => return Ok(None),
            WizardKey::Back | WizardKey::Left | WizardKey::Right => {}
            WizardKey::Up => cursor = cursor.saturating_sub(1),
            WizardKey::Down => {
                if cursor + 1 < choices.len() {
                    cursor += 1;
                }
            }
            WizardKey::Toggle => {
                if choices[cursor].info.name != "host" {
                    choices[cursor].selected = !choices[cursor].selected;
                }
            }
            WizardKey::Enter => break,
        }
    }

    let mut roles = selected_role_names(&choices);
    if !roles.iter().any(|role| role == "host") {
        roles.insert(0, "host".to_owned());
    }
    let mut assignments = default_assignments(&roles, installed);
    cursor = 0;

    loop {
        terminal.render(&render_engine_picker(
            project_root,
            installed,
            &roles,
            &assignments,
            cursor,
        ))?;
        match read_key()? {
            WizardKey::Abort => return Ok(None),
            WizardKey::Back => {
                return run_wizard(project_root, scan, installed, default_plan);
            }
            WizardKey::Up => cursor = cursor.saturating_sub(1),
            WizardKey::Down => {
                if cursor + 1 < roles.len() {
                    cursor += 1;
                }
            }
            WizardKey::Left => cycle_assignment(&mut assignments, &roles[cursor], -1),
            WizardKey::Right | WizardKey::Toggle => {
                cycle_assignment(&mut assignments, &roles[cursor], 1);
            }
            WizardKey::Enter => break,
        }
    }

    let plan = roles
        .iter()
        .map(|name| RolePlan {
            name: name.clone(),
            engine: *assignments.get(name).unwrap_or(&DEFAULT_ENGINE),
        })
        .collect::<Vec<_>>();

    loop {
        terminal.render(&render_confirm(project_root, scan, &plan))?;
        match read_key()? {
            WizardKey::Abort => return Ok(None),
            WizardKey::Back => {
                return run_wizard(project_root, scan, installed, default_plan);
            }
            WizardKey::Enter => return Ok(Some(plan)),
            WizardKey::Up
            | WizardKey::Down
            | WizardKey::Left
            | WizardKey::Right
            | WizardKey::Toggle => {}
        }
    }
}

/// Raw-mode terminal wrapper for full-screen interactive pickers.
///
/// Used by `cr init`'s role/engine wizard *and* by the REPL `/resume`
/// session picker. Owns the SIGINT/SIGTERM trap so a Ctrl-C while
/// drawing leaves the terminal in a sane state (cooked mode + cursor
/// visible) even if Drop didn't run.
#[derive(Debug)]
pub(crate) struct WizardTerminal {
    stdout: std::io::Stdout,
    raw_active: Arc<AtomicBool>,
    signal_handle: Option<SignalHandle>,
    signal_thread: Option<JoinHandle<()>>,
}

impl WizardTerminal {
    pub(crate) fn enter() -> Result<Self> {
        let raw_active = Arc::new(AtomicBool::new(true));
        let mut signals = Signals::new([SIGINT, SIGTERM])?;
        let signal_handle = signals.handle();
        terminal::enable_raw_mode()?;
        let signal_active = Arc::clone(&raw_active);
        let signal_thread = thread::spawn(move || {
            for signal in signals.forever() {
                if !signal_active.swap(false, Ordering::SeqCst) {
                    continue;
                }
                let _ = terminal::disable_raw_mode();
                let mut stdout = std::io::stdout();
                let _ = execute!(stdout, Show);
                std::process::exit(signal_exit_code(signal));
            }
        });
        let mut stdout = std::io::stdout();
        if let Err(error) = execute!(stdout, Hide) {
            raw_active.store(false, Ordering::SeqCst);
            let _ = terminal::disable_raw_mode();
            signal_handle.close();
            let _ = signal_thread.join();
            return Err(error.into());
        }
        Ok(Self {
            stdout,
            raw_active,
            signal_handle: Some(signal_handle),
            signal_thread: Some(signal_thread),
        })
    }

    pub(crate) fn render(&mut self, body: &str) -> Result<()> {
        // Raw mode disables ONLCR — bare `\n` only moves the cursor down,
        // it does NOT return to column 0. Picker bodies are built with
        // `writeln!` (LF only); without translation each row starts at
        // the column where the previous row ended, and the layout
        // marches diagonally down-right across the screen. This is the
        // "garbled picker" bug users reported in 0.1.7 / 0.1.8 / 0.1.9.
        queue!(self.stdout, MoveTo(0, 0), Clear(ClearType::All))?;
        let crlf = body.replace('\n', "\r\n");
        self.stdout.write_all(crlf.as_bytes())?;
        self.stdout.flush()?;
        Ok(())
    }
}

impl Drop for WizardTerminal {
    fn drop(&mut self) {
        self.raw_active.store(false, Ordering::SeqCst);
        let _ = terminal::disable_raw_mode();
        let _ = execute!(self.stdout, Show);
        if let Some(handle) = self.signal_handle.take() {
            handle.close();
        }
        if let Some(thread) = self.signal_thread.take() {
            let _ = thread.join();
        }
    }
}

fn signal_exit_code(signal: i32) -> i32 {
    128 + signal
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WizardKey {
    Up,
    Down,
    Left,
    Right,
    Toggle,
    Enter,
    Back,
    Abort,
}

pub(crate) fn read_key() -> Result<WizardKey> {
    loop {
        let event = event::read()?;
        if let Some(key) = wizard_key_from_event(&event) {
            return Ok(key);
        }
    }
}

fn wizard_key_from_event(event: &Event) -> Option<WizardKey> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') => return Some(WizardKey::Abort),
            KeyCode::Char('j' | 'm') => return Some(WizardKey::Enter),
            _ => {}
        }
    }
    Some(match key.code {
        KeyCode::Up | KeyCode::Char('k') => WizardKey::Up,
        KeyCode::Down | KeyCode::Char('j') => WizardKey::Down,
        KeyCode::Left | KeyCode::Char('h') => WizardKey::Left,
        KeyCode::Right | KeyCode::Char('l') => WizardKey::Right,
        KeyCode::Char(' ') => WizardKey::Toggle,
        KeyCode::Enter | KeyCode::Char('\n' | '\r') => WizardKey::Enter,
        KeyCode::Esc => WizardKey::Back,
        KeyCode::Char('q') => WizardKey::Abort,
        _ => return None,
    })
}

#[derive(Debug, Clone)]
struct RoleChoice {
    info: RoleInfo,
    selected: bool,
}

fn build_role_choices(default_plan: &[RolePlan]) -> Vec<RoleChoice> {
    let selected: Vec<&str> = default_plan.iter().map(|role| role.name.as_str()).collect();
    ROLE_CATALOG
        .iter()
        .map(|info| RoleChoice {
            info: *info,
            selected: info.name == "host" || selected.contains(&info.name),
        })
        .collect()
}

fn selected_role_names(choices: &[RoleChoice]) -> Vec<String> {
    choices
        .iter()
        .filter(|choice| choice.selected)
        .map(|choice| choice.info.name.to_owned())
        .collect()
}

fn default_assignments(roles: &[String], installed: &InstalledEngines) -> HashMap<String, Engine> {
    roles
        .iter()
        .map(|role| (role.clone(), default_engine_for_role(role, installed)))
        .collect()
}

fn cycle_assignment(assignments: &mut HashMap<String, Engine>, role: &str, direction: i8) {
    const ENGINES: &[Engine] = &[Engine::Cc, Engine::Codex, Engine::Gemini];
    let current = assignments.get(role).copied().unwrap_or(DEFAULT_ENGINE);
    let index = ENGINES
        .iter()
        .position(|engine| *engine == current)
        .unwrap_or(0);
    let next = if direction < 0 {
        (index + ENGINES.len() - 1) % ENGINES.len()
    } else {
        (index + 1) % ENGINES.len()
    };
    assignments.insert(role.to_owned(), ENGINES[next]);
}

/// Thin wrapper around [`crate::engines::Engines::detect`] kept under
/// its old name so existing call sites compile unchanged.
fn detect_installed_engines() -> InstalledEngines {
    crate::engines::Engines::detect()
}

#[cfg(test)]
mod tests;
