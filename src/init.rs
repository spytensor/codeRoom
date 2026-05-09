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
use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Color, Stylize};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};

use crate::adapter::Engine;
use crate::config::{CODEROOM_DIR, CONFIG_FILE, ROLES_DIR};
use crate::detect::{self, StackSignal};

const DEFAULT_HOST_PRIORS: &str = include_str!("init_defaults/host.md");
const DEFAULT_SHARED_PRIORS: &str = include_str!("init_defaults/shared.md");
const DEFAULT_GITIGNORE: &str = include_str!("init_defaults/gitignore");
const DEFAULT_ROLE_TEMPLATE: &str = include_str!("init_defaults/role_template.md");

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
    preview: &'static [&'static str],
    estimated_tokens: &'static str,
}

const ROLE_CATALOG: &[RoleInfo] = &[
    RoleInfo {
        name: "host",
        description: "orchestrates requests and keeps the room coherent",
        preview: &[
            "routes bare messages, pulls in specialists, and synthesizes",
            "the final answer without pretending to own risky decisions",
        ],
        estimated_tokens: "2.4k",
    },
    RoleInfo {
        name: "backend",
        description: "APIs, services, storage boundaries",
        preview: &[
            "knows service contracts, persistence rules, migrations,",
            "background jobs, queues, and backend-only gotchas",
        ],
        estimated_tokens: "3.4k",
    },
    RoleInfo {
        name: "frontend",
        description: "UI, components, routing, client-side state",
        preview: &[
            "knows component conventions, accessibility expectations,",
            "design tokens, state boundaries, and browser-side regressions",
        ],
        estimated_tokens: "2.8k",
    },
    RoleInfo {
        name: "security",
        description: "authn, authz, threat modeling",
        preview: &[
            "checks permission boundaries, secrets, auth flows, injection",
            "surfaces, data exposure, and unsafe operational shortcuts",
        ],
        estimated_tokens: "3.1k",
    },
    RoleInfo {
        name: "data",
        description: "schemas, migrations, query patterns",
        preview: &[
            "tracks schemas, migrations, data quality assumptions,",
            "query plans, retention rules, and reporting contracts",
        ],
        estimated_tokens: "2.6k",
    },
    RoleInfo {
        name: "devops",
        description: "CI/CD, infra, deploys, runtime health",
        preview: &[
            "owns deployment shape, environment drift, observability,",
            "container boundaries, and operational recovery paths",
        ],
        estimated_tokens: "2.7k",
    },
    RoleInfo {
        name: "ci",
        description: "workflows, checks, release gates",
        preview: &[
            "keeps test gates, release jobs, artifact generation,",
            "and flaky workflow recovery grounded in the repo",
        ],
        estimated_tokens: "2.1k",
    },
    RoleInfo {
        name: "qa",
        description: "test strategy, edge cases, regression risk",
        preview: &[
            "thinks in scenarios, missing coverage, edge conditions,",
            "and the checks that should fail before users do",
        ],
        estimated_tokens: "2.4k",
    },
    RoleInfo {
        name: "docs",
        description: "technical writing, examples, API reference",
        preview: &[
            "keeps installation, usage, architecture, and migration",
            "docs accurate enough that users trust the tool",
        ],
        estimated_tokens: "1.9k",
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
        "· each role can use a different CLI".dark_grey()
    );
    for engine in [Engine::Cc, Engine::Codex, Engine::Gemini] {
        let label = engine_label(engine);
        if installed.is_present(engine) {
            println!(
                "  {} {:<13} {}",
                "✓".green(),
                label.with(Color::White),
                engine_install_hint(engine).dark_grey()
            );
        } else {
            println!(
                "  {} {:<13} {} {}",
                "×".red(),
                label.dark_grey(),
                "not installed ·".dark_grey(),
                engine_install_hint(engine).yellow()
            );
        }
    }
}

fn print_role_summary(roles: &[RolePlan]) {
    println!(
        "{} {}",
        "assign roles".bold(),
        "· generated from detected project signals".dark_grey()
    );
    println!(
        "  {:<13} {:<13} {}",
        "role".dark_grey(),
        "engine".dark_grey(),
        "focus".dark_grey()
    );
    for role in roles {
        let info = role_info(&role.name);
        println!(
            "  {:<13} {:<13} {}",
            format!("@{}", role.name).with(role_color(&role.name)),
            engine_label(role.engine).with(engine_color(role.engine)),
            info.description.dark_grey()
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

#[derive(Debug)]
struct WizardTerminal {
    stdout: std::io::Stdout,
}

impl WizardTerminal {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, Hide)?;
        Ok(Self { stdout })
    }

    fn render(&mut self, body: &str) -> Result<()> {
        queue!(self.stdout, MoveTo(0, 0), Clear(ClearType::All))?;
        self.stdout.write_all(body.as_bytes())?;
        self.stdout.flush()?;
        Ok(())
    }
}

impl Drop for WizardTerminal {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(self.stdout, Show);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WizardKey {
    Up,
    Down,
    Left,
    Right,
    Toggle,
    Enter,
    Back,
    Abort,
}

fn read_key() -> Result<WizardKey> {
    loop {
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('c') => return Ok(WizardKey::Abort),
                    KeyCode::Char('j' | 'm') => return Ok(WizardKey::Enter),
                    _ => {}
                }
            }
            return Ok(match key.code {
                KeyCode::Up | KeyCode::Char('k') => WizardKey::Up,
                KeyCode::Down | KeyCode::Char('j') => WizardKey::Down,
                KeyCode::Left | KeyCode::Char('h') => WizardKey::Left,
                KeyCode::Right | KeyCode::Char('l') => WizardKey::Right,
                KeyCode::Char(' ') => WizardKey::Toggle,
                KeyCode::Enter | KeyCode::Char('\n' | '\r') => WizardKey::Enter,
                KeyCode::Esc => WizardKey::Back,
                KeyCode::Char('q') => WizardKey::Abort,
                _ => continue,
            });
        }
    }
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

fn render_role_picker(
    project_root: &Path,
    scan: &detect::ProjectScan,
    choices: &[RoleChoice],
    cursor: usize,
) -> String {
    let project_name = project_name(project_root);
    let selected_count = choices.iter().filter(|choice| choice.selected).count();
    let mut out = String::new();

    push_header(
        &mut out,
        &project_name,
        "pick roles",
        "space toggles · enter continues · esc backs out · q quits",
    );
    push_scan_compact(&mut out, scan);
    let _ = writeln!(out);

    for (index, choice) in choices.iter().enumerate() {
        let marker = if index == cursor { "›" } else { " " };
        let check = if choice.selected { "[x]" } else { "[ ]" };
        let lock = if choice.info.name == "host" {
            " required"
        } else {
            ""
        };
        let row = format!(
            "{marker} {check} ● @{:<10} {:<52} {:>5}{lock}",
            choice.info.name, choice.info.description, choice.info.estimated_tokens
        );
        if index == cursor {
            let _ = writeln!(out, "{}", row.with(Color::White).on(Color::DarkGrey));
            for line in choice.info.preview {
                let _ = writeln!(out, "      {} {}", "└─".dark_grey(), line.dark_grey());
            }
        } else {
            let role = format!("@{}", choice.info.name).with(role_color(choice.info.name));
            let _ = writeln!(
                out,
                "{marker} {check} ● {:<19} {:<52} {:>5}{lock}",
                role,
                choice.info.description.dark_grey(),
                choice.info.estimated_tokens.dark_grey()
            );
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{}",
        format!("{selected_count} selected · host is always present · enter continues").dark_grey()
    );
    out
}

fn render_engine_picker(
    project_root: &Path,
    installed: &InstalledEngines,
    roles: &[String],
    assignments: &HashMap<String, Engine>,
    cursor: usize,
) -> String {
    let project_name = project_name(project_root);
    let mut out = String::new();

    push_header(
        &mut out,
        &project_name,
        "assign engines",
        "↑/↓ moves · ←/→ cycles engine · enter continues · esc goes back",
    );
    push_engine_status_compact(&mut out, installed);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  {:<14} {:<15} {:<18} {}",
        "role".dark_grey(),
        "engine".dark_grey(),
        "model".dark_grey(),
        "note".dark_grey()
    );

    for (index, role) in roles.iter().enumerate() {
        let engine = *assignments.get(role).unwrap_or(&DEFAULT_ENGINE);
        let note = engine_note(engine, installed);
        let row = format!(
            "{} {:<13} ‹ {:<10} › {:<18} {}",
            if index == cursor { "›" } else { " " },
            format!("@{role}"),
            engine_label(engine),
            model_label(engine),
            note
        );
        if index == cursor {
            let _ = writeln!(out, "{}", row.with(Color::White).on(Color::DarkGrey));
        } else {
            let _ = writeln!(
                out,
                "  {:<13} ‹ {:<10} › {:<18} {}",
                format!("@{role}").with(role_color(role)),
                engine_label(engine).with(engine_color(engine)),
                model_label(engine).dark_grey(),
                note.dark_grey()
            );
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{}",
        "defaults are editable later in .coderoom/config.toml".dark_grey()
    );
    out
}

fn render_confirm(project_root: &Path, scan: &detect::ProjectScan, plan: &[RolePlan]) -> String {
    let project_name = project_name(project_root);
    let coderoom_dir = project_root.join(CODEROOM_DIR);
    let mut out = String::new();

    push_header(
        &mut out,
        &project_name,
        "ready to write",
        "nothing is written until Enter",
    );

    let _ = writeln!(out, "will create:");
    let _ = writeln!(out);
    push_tree_preview(&mut out, &coderoom_dir, plan);
    let _ = writeln!(out);
    print_role_plan_to_buffer(&mut out, plan);

    if let Some(line_count) = scan.existing_claude_md() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "{} found existing {} ({} lines).",
            "!".yellow(),
            "CLAUDE.md".bold(),
            line_count
        );
        let _ = writeln!(
            out,
            "  {}",
            "coderoom will not touch it; split assistance can land separately.".dark_grey()
        );
    }

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{}",
        "enter writes · esc goes back · q aborts".dark_grey()
    );
    out
}

fn push_header(out: &mut String, project_name: &str, title: &str, subtitle: &str) {
    let _ = writeln!(
        out,
        "{} {} {}",
        title.bold(),
        "·".dark_grey(),
        format!("setting up coderoom in {project_name}").dark_grey()
    );
    let _ = writeln!(out, "{}", subtitle.dark_grey());
    let _ = writeln!(out);
}

fn push_scan_compact(out: &mut String, scan: &detect::ProjectScan) {
    if scan.stack.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            "detected: no stack signals at project root".dark_grey()
        );
        return;
    }
    let labels = scan
        .stack
        .iter()
        .take(4)
        .map(human_label)
        .collect::<Vec<_>>()
        .join(" · ");
    let suffix = if scan.stack.len() > 4 { " · …" } else { "" };
    let _ = writeln!(
        out,
        "{} {}{}",
        "detected:".dark_grey(),
        labels,
        suffix.dark_grey()
    );
}

fn push_engine_status_compact(out: &mut String, installed: &InstalledEngines) {
    let _ = writeln!(out, "detected on your system:");
    for engine in [Engine::Cc, Engine::Codex, Engine::Gemini] {
        if installed.is_present(engine) {
            let _ = writeln!(
                out,
                "  {} {:<13} {}",
                "✓".green(),
                engine_label(engine).with(engine_color(engine)),
                "installed".dark_grey()
            );
        } else {
            let _ = writeln!(
                out,
                "  {} {:<13} {} {}",
                "×".red(),
                engine_label(engine).dark_grey(),
                "not installed ·".dark_grey(),
                engine_install_hint(engine).yellow()
            );
        }
    }
}

fn push_tree_preview(out: &mut String, coderoom_dir: &Path, plan: &[RolePlan]) {
    let dirname = coderoom_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(CODEROOM_DIR);
    let _ = writeln!(out, "{dirname}/");
    let _ = writeln!(
        out,
        "├─ config.toml              {}",
        format!("{} roles", plan.len()).dark_grey()
    );
    let _ = writeln!(
        out,
        "├─ shared.md                {}",
        "project-wide priors".dark_grey()
    );
    let _ = writeln!(out, "├─ roles/");
    for (index, role) in plan.iter().enumerate() {
        let branch = if index + 1 == plan.len() {
            "└─"
        } else {
            "├─"
        };
        let _ = writeln!(
            out,
            "│  {branch} {:<18} {}",
            format!("{}.md", role.name).with(role_color(&role.name)),
            engine_label(role.engine).dark_grey()
        );
    }
    let _ = writeln!(out, "└─ .gitignore");
}

fn print_role_plan_to_buffer(out: &mut String, plan: &[RolePlan]) {
    let _ = writeln!(
        out,
        "  {:<14} {:<12} {}",
        "role".dark_grey(),
        "engine".dark_grey(),
        "focus".dark_grey()
    );
    for role in plan {
        let info = role_info(&role.name);
        let _ = writeln!(
            out,
            "  {:<14} {:<12} {}",
            format!("@{}", role.name).with(role_color(&role.name)),
            engine_label(role.engine).with(engine_color(role.engine)),
            info.description.dark_grey()
        );
    }
}

/// Materialize the `.coderoom/` skeleton on disk. Each role gets a
/// templated priors file with `{ROLE}` substituted.
fn write_all(coderoom_dir: &Path, roles: &[RolePlan]) -> Result<()> {
    let roles_dir = coderoom_dir.join(ROLES_DIR);
    std::fs::create_dir_all(&roles_dir)
        .with_context(|| format!("creating {}", roles_dir.display()))?;

    write_file(&coderoom_dir.join(CONFIG_FILE), &render_config(roles))?;
    write_file(&coderoom_dir.join("shared.md"), DEFAULT_SHARED_PRIORS)?;
    for role in roles {
        let path = roles_dir.join(format!("{}.md", role.name));
        let body = if role.name == "host" {
            DEFAULT_HOST_PRIORS.to_string()
        } else {
            DEFAULT_ROLE_TEMPLATE.replace("{ROLE}", &role.name)
        };
        write_file(&path, &body)?;
    }
    write_file(&coderoom_dir.join(".gitignore"), DEFAULT_GITIGNORE)?;
    Ok(())
}

fn render_config(roles: &[RolePlan]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# CodeRoom project config. See https://github.com/spytensor/codeRoom for docs."
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "# Engine used by any role that doesn't override.");
    let _ = writeln!(
        out,
        "# Options: \"cc\" (Claude Code), \"codex\", \"gemini\"."
    );
    let _ = writeln!(out, "default_engine = \"{}\"", DEFAULT_ENGINE.as_str());
    let _ = writeln!(out);
    let _ = writeln!(out, "# Optional default model id; engine-specific.");
    let _ = writeln!(out, "# default_model = \"opus\"");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "# Per-role budget cap in USD, fed to each engine's native budget flag."
    );
    let _ = writeln!(out, "budget_per_role_usd = 0.50");
    let _ = writeln!(out);
    let _ = writeln!(out, "# Role that catches un-addressed text in the REPL.");
    let _ = writeln!(out, "host_role = \"host\"");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "# Per-role overrides. Priors live in .coderoom/roles/<name>.md."
    );
    for role in roles {
        let _ = writeln!(out, "[roles.{}]", role.name);
        if role.engine != DEFAULT_ENGINE {
            let _ = writeln!(out, "engine = \"{}\"", role.engine.as_str());
        }
        let _ = writeln!(out);
    }
    out
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Human-readable bullet for a [`StackSignal`], used in the scan
/// summary. Kept here (not on the type) so the wording can evolve
/// independently of the detector's internals.
fn human_label(signal: &StackSignal) -> String {
    match signal {
        StackSignal::CargoToml => "Cargo.toml (Rust)".into(),
        StackSignal::GoMod => "go.mod (Go)".into(),
        StackSignal::PackageJson {
            has_ui_framework: true,
        } => "package.json (with UI framework)".into(),
        StackSignal::PackageJson {
            has_ui_framework: false,
        } => "package.json (no UI framework detected)".into(),
        StackSignal::PythonProject => "Python project (requirements.txt or pyproject.toml)".into(),
        StackSignal::JvmProject => "JVM project (pom.xml or build.gradle)".into(),
        StackSignal::Migrations => "migrations/ or db/ directory".into(),
        StackSignal::Prisma => "prisma/ directory".into(),
        StackSignal::GithubWorkflows => ".github/workflows/".into(),
        StackSignal::Dockerfile => "Dockerfile".into(),
        StackSignal::Terraform => "terraform/".into(),
        StackSignal::Pulumi => "pulumi/".into(),
        StackSignal::Kubernetes => "k8s/ or kubernetes/".into(),
        StackSignal::ExistingClaudeMd { line_count } => format!("CLAUDE.md ({line_count} lines)"),
    }
}

/// Result of probing `claude`/`codex`/`gemini` on `$PATH`.
struct InstalledEngines {
    cc: bool,
    codex: bool,
    gemini: bool,
}

impl InstalledEngines {
    fn is_present(&self, engine: Engine) -> bool {
        match engine {
            Engine::Cc => self.cc,
            Engine::Codex => self.codex,
            Engine::Gemini => self.gemini,
        }
    }
}

fn project_name(project_root: &Path) -> String {
    project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("(this project)")
        .to_owned()
}

fn role_info(name: &str) -> RoleInfo {
    ROLE_CATALOG
        .iter()
        .copied()
        .find(|info| info.name == name)
        .unwrap_or(RoleInfo {
            name: "custom",
            description: "project-specific specialist",
            preview: &["custom role loaded from .coderoom/roles/<role>.md"],
            estimated_tokens: "2.0k",
        })
}

fn role_color(role: &str) -> Color {
    match role {
        "host" => Color::Cyan,
        "backend" => Color::Green,
        "frontend" => Color::Rgb {
            r: 255,
            g: 168,
            b: 120,
        },
        "security" => Color::Rgb {
            r: 255,
            g: 140,
            b: 140,
        },
        "data" => Color::Magenta,
        "devops" => Color::DarkYellow,
        "ci" => Color::Blue,
        "qa" => Color::Yellow,
        "docs" => Color::DarkGrey,
        _ => Color::White,
    }
}

fn engine_color(engine: Engine) -> Color {
    match engine {
        Engine::Cc => Color::White,
        Engine::Codex => Color::Blue,
        Engine::Gemini => Color::Magenta,
    }
}

fn engine_label(engine: Engine) -> &'static str {
    match engine {
        Engine::Cc => "claude-code",
        Engine::Codex => "codex",
        Engine::Gemini => "gemini-cli",
    }
}

fn model_label(engine: Engine) -> &'static str {
    match engine {
        Engine::Cc => "claude default",
        Engine::Codex => "codex default",
        Engine::Gemini => "gemini default",
    }
}

fn engine_install_hint(engine: Engine) -> &'static str {
    match engine {
        Engine::Cc => "docs.anthropic.com/claude-code",
        Engine::Codex => "github.com/openai/codex",
        Engine::Gemini => "github.com/google/gemini-cli",
    }
}

fn engine_note(engine: Engine, installed: &InstalledEngines) -> &'static str {
    if installed.is_present(engine) {
        "ready"
    } else {
        "install before cr start"
    }
}

/// Probe `$PATH` for the three engine binaries. Each probe runs
/// `<bin> --version` once with all I/O captured, ~30 ms per call —
/// init runs once per project so this is cheap.
fn detect_installed_engines() -> InstalledEngines {
    InstalledEngines {
        cc: bin_present("claude"),
        codex: bin_present("codex"),
        gemini: bin_present("gemini"),
    }
}

fn bin_present(name: &str) -> bool {
    use std::process::Command;
    Command::new(name)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[test]
    fn init_yes_creates_minimal_valid_layout() {
        let tmp = TempDir::new().unwrap();
        run(tmp.path(), InitOptions::auto()).expect("auto init succeeds in fresh dir");

        let coderoom = tmp.path().join(CODEROOM_DIR);
        assert!(coderoom.is_dir());
        assert!(coderoom.join(CONFIG_FILE).is_file());
        assert!(coderoom.join("shared.md").is_file());
        assert!(coderoom.join(ROLES_DIR).join("host.md").is_file());
        assert!(coderoom.join(".gitignore").is_file());
    }

    #[test]
    fn init_yes_output_passes_config_validation() {
        let tmp = TempDir::new().unwrap();
        run(tmp.path(), InitOptions::auto()).expect("init");
        let cfg = Config::load_test(tmp.path()).expect("init output should be a valid config");
        assert_eq!(cfg.host_role, "host");
        assert!(cfg.is_host("host"));
    }

    #[test]
    fn init_refuses_to_overwrite_existing_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(CODEROOM_DIR)).unwrap();
        let err = run(tmp.path(), InitOptions::auto()).expect_err("should refuse to overwrite");
        assert!(err.to_string().contains("already exists"), "got: {err}");
    }

    #[test]
    fn detected_stack_creates_extra_roles_in_config() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        run(tmp.path(), InitOptions::auto()).expect("init");

        let cfg = Config::load_test(tmp.path()).expect("valid config");
        // Cargo.toml → host + backend + security
        assert!(cfg.roles.contains_key("host"));
        assert!(cfg.roles.contains_key("backend"));
        assert!(cfg.roles.contains_key("security"));

        let coderoom = tmp.path().join(CODEROOM_DIR);
        assert!(coderoom.join(ROLES_DIR).join("backend.md").is_file());
        assert!(coderoom.join(ROLES_DIR).join("security.md").is_file());
    }

    #[test]
    fn role_template_substitutes_role_name() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("go.mod"), "module x\n").unwrap();
        run(tmp.path(), InitOptions::auto()).expect("init");

        let backend_priors = std::fs::read_to_string(
            tmp.path()
                .join(CODEROOM_DIR)
                .join(ROLES_DIR)
                .join("backend.md"),
        )
        .unwrap();
        // Template's `{ROLE}` placeholder should be replaced.
        assert!(!backend_priors.contains("{ROLE}"));
        assert!(backend_priors.contains("@backend"));
    }

    #[test]
    fn planned_files_lists_in_render_order() {
        let coderoom = PathBuf::from("/tmp/p/.coderoom");
        let paths = planned_files(
            &coderoom,
            &[
                RolePlan {
                    name: "host".into(),
                    engine: Engine::Cc,
                },
                RolePlan {
                    name: "backend".into(),
                    engine: Engine::Cc,
                },
            ],
        );
        let display: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(
            display,
            vec![
                "/tmp/p/.coderoom/config.toml",
                "/tmp/p/.coderoom/shared.md",
                "/tmp/p/.coderoom/roles/host.md",
                "/tmp/p/.coderoom/roles/backend.md",
                "/tmp/p/.coderoom/.gitignore",
            ]
        );
    }
}
