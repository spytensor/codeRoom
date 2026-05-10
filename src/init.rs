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
use crate::config::{Config, CODEROOM_DIR, CONFIG_FILE, ROLES_DIR};
use crate::detect::{self, StackSignal};
use crate::output;
use crate::role::{self, RoleAddition};

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

/// Visible-cell budget used by the role picker layout.
///
/// `    [x] ● @<name>....description...`
///  ^^^^^^^^^^^^^^^^^^^
///  4    4   2  NAME_VISIBLE
///
/// The description fills whatever's left of the terminal width,
/// truncated (not wrapped) so each row stays exactly one line.
const NAME_VISIBLE: usize = 12;
const PICKER_PREFIX_VISIBLE: usize = 4 + 4 + 2 + NAME_VISIBLE;
const PICKER_RIGHT_MARGIN: usize = 2;
const PICKER_DEFAULT_COLS: u16 = 80;
const PICKER_MIN_DESC: usize = 16;

/// Detect terminal columns; fall back to 80 when unavailable
/// (non-TTY / piped output).
fn picker_columns() -> usize {
    terminal::size()
        .map_or(PICKER_DEFAULT_COLS, |(cols, _)| cols)
        .max(40) as usize
}

/// Render one row of the picker.
///
/// Each row is exactly one terminal line: prefix is fixed-width, the
/// description gets truncated with `…` so it can never wrap. Styling is
/// applied **after** all visible-width math so SGR escapes never leak
/// into padding budgets.
fn picker_row(
    info: &RoleInfo,
    selected: bool,
    is_cursor: bool,
    columns: usize,
    extra_tag: Option<&str>,
) -> String {
    let cursor_glyph = if is_cursor { "  > " } else { "    " };
    let check = if selected { "[x] " } else { "[ ] " };

    // `@<name>` left-padded to NAME_VISIBLE visible cells. Pad PLAIN,
    // colour after — that's the bug the previous picker tripped on.
    let name_plain = format!("@{:<width$}", info.name, width = NAME_VISIBLE - 1);

    let mut desc_plain = info.description.to_owned();
    if let Some(tag) = extra_tag {
        desc_plain.push_str(" · ");
        desc_plain.push_str(tag);
    }
    let desc_budget = columns
        .saturating_sub(PICKER_PREFIX_VISIBLE)
        .saturating_sub(PICKER_RIGHT_MARGIN)
        .max(PICKER_MIN_DESC);
    let desc_truncated = output::truncate_visible(&desc_plain, desc_budget);

    let paint = role_color(info.name);
    format!(
        "{}{}{} {} {}",
        cursor_glyph.with(output::PROMPT),
        check.with(if is_cursor { output::EM } else { output::TEXT }),
        "●".with(paint),
        name_plain.with(paint).bold(),
        desc_truncated.with(output::DIM),
    )
}

fn render_role_picker(
    project_root: &Path,
    scan: &detect::ProjectScan,
    choices: &[RoleChoice],
    cursor: usize,
) -> String {
    let project_name = project_name(project_root);
    let selected_count = choices.iter().filter(|choice| choice.selected).count();
    let columns = picker_columns();
    let mut out = String::new();

    push_header(
        &mut out,
        &project_name,
        "pick roles",
        "space toggles · ↑↓ moves · enter continues · esc backs out",
    );
    push_scan_compact(&mut out, scan);
    let _ = writeln!(out);

    for (index, choice) in choices.iter().enumerate() {
        let extra_tag = if choice.info.name == "host" {
            Some("required")
        } else {
            None
        };
        let _ = writeln!(
            out,
            "{}",
            picker_row(
                &choice.info,
                choice.selected,
                index == cursor,
                columns,
                extra_tag,
            )
        );
    }

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{}",
        format!("{selected_count} selected · host is always present · enter continues")
            .with(output::DIM)
    );
    out
}

fn render_role_expansion_picker(
    project_root: &Path,
    scan: &detect::ProjectScan,
    choices: &[RoleChoice],
    cursor: usize,
) -> String {
    let project_name = project_name(project_root);
    let selected_count = choices
        .iter()
        .filter(|choice| choice.selected && choice.info.name != "host")
        .count();
    let columns = picker_columns();
    let mut out = String::new();

    push_header(
        &mut out,
        &project_name,
        "suggest roles",
        "space toggles · ↑↓ moves · enter adds selected · esc skips",
    );
    push_scan_compact(&mut out, scan);
    let _ = writeln!(
        out,
        "{}",
        "CodeRoom found only @host. Choose the specialists to add:".with(output::DIM)
    );
    let _ = writeln!(out);

    for (index, choice) in choices.iter().enumerate() {
        let extra_tag = if choice.info.name == "host" {
            Some("existing")
        } else {
            None
        };
        let _ = writeln!(
            out,
            "{}",
            picker_row(
                &choice.info,
                choice.selected,
                index == cursor,
                columns,
                extra_tag,
            )
        );
    }

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{}",
        format!("{selected_count} new role(s) selected · enter writes config and priors")
            .with(output::DIM)
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
    // Pad PLAIN, style after — same fix as the role picker (`StyledContent`
    // includes SGR escapes when formatted, so `{:<N}` padding leaks into
    // the visible row width and the layout bleeds across lines).
    const ROLE_W: usize = 13;
    const ENGINE_W: usize = 10;
    const MODEL_W: usize = 18;

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
    let header = format!(
        "  {:<ROLE_W$} ‹ {:<ENGINE_W$} › {:<MODEL_W$} {}",
        "role", "engine", "model", "note"
    );
    let _ = writeln!(out, "{}", header.with(output::DIM));

    for (index, role) in roles.iter().enumerate() {
        let engine = *assignments.get(role).unwrap_or(&DEFAULT_ENGINE);
        let note = engine_note(engine, installed);
        let cursor_glyph = if index == cursor { "  > " } else { "    " };
        let role_plain = format!("@{role:<width$}", width = ROLE_W - 1);
        let engine_plain = format!("{:<ENGINE_W$}", engine_label(engine));
        let model_plain = format!("{:<MODEL_W$}", model_label(engine));
        let _ = writeln!(
            out,
            "{}{} ‹ {} › {} {}",
            cursor_glyph.with(output::PROMPT),
            role_plain.with(role_color(role)).bold(),
            engine_plain.with(engine_color(engine)),
            model_plain.with(output::DIM),
            note.with(output::DIM),
        );
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
        let label_padded = format!("{:<13}", engine_label(engine));
        if installed.is_present(engine) {
            let _ = writeln!(
                out,
                "  {} {} {}",
                "✓".with(output::OK),
                label_padded.with(engine_color(engine)),
                "installed".with(output::DIM),
            );
        } else {
            let _ = writeln!(
                out,
                "  {} {} {} {}",
                "✗".with(output::BAD),
                label_padded.with(output::DIM),
                "not installed ·".with(output::DIM),
                engine_install_hint(engine).with(output::WARN),
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
        format!("{} roles", plan.len()).with(output::DIM)
    );
    let _ = writeln!(
        out,
        "├─ shared.md                {}",
        "project-wide priors".with(output::DIM)
    );
    let _ = writeln!(out, "├─ roles/");
    for (index, role) in plan.iter().enumerate() {
        let branch = if index + 1 == plan.len() {
            "└─"
        } else {
            "├─"
        };
        let role_filename = format!("{}.md", role.name);
        let role_filename_padded = format!("{role_filename:<18}");
        let _ = writeln!(
            out,
            "│  {branch} {} {}",
            role_filename_padded.with(role_color(&role.name)),
            engine_label(role.engine).with(output::DIM),
        );
    }
    let _ = writeln!(out, "└─ .gitignore");
}

fn print_role_plan_to_buffer(out: &mut String, plan: &[RolePlan]) {
    let header = format!("  {:<14} {:<12} {}", "role", "engine", "focus");
    let _ = writeln!(out, "{}", header.with(output::DIM));
    for role in plan {
        let info = role_info(&role.name);
        let role_token = format!("@{:<width$}", role.name, width = 13);
        let engine_padded = format!("{:<12}", engine_label(role.engine));
        let _ = writeln!(
            out,
            "  {} {} {}",
            role_token.with(role_color(&role.name)),
            engine_padded.with(engine_color(role.engine)),
            info.description.with(output::DIM),
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
            render_role_priors(&role.name, roles)
        };
        write_file(&path, &body)?;
    }
    write_file(&coderoom_dir.join(".gitignore"), DEFAULT_GITIGNORE)?;
    Ok(())
}

fn render_role_priors(role_name: &str, roles: &[RolePlan]) -> String {
    let peers = roles
        .iter()
        .filter(|role| role.name != role_name)
        .map(|role| format!("@{}", role.name))
        .collect::<Vec<_>>();
    DEFAULT_ROLE_TEMPLATE
        .replace("{ROLE}", role_name)
        .replace("{HOST}", "host")
        .replace(
            "{PEERS}",
            &if peers.is_empty() {
                "(none configured yet)".to_owned()
            } else {
                peers.join(", ")
            },
        )
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

/// Re-exported under the old name so the rest of `init.rs` doesn't have
/// to change. Single source of truth lives in `crate::engines`.
type InstalledEngines = crate::engines::Engines;

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
        })
}

fn role_color(role: &str) -> Color {
    output::role_color(role, WIZARD_HOST_ROLE)
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

/// Thin wrapper around [`crate::engines::Engines::detect`] kept under
/// its old name so existing call sites compile unchanged.
fn detect_installed_engines() -> InstalledEngines {
    crate::engines::Engines::detect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RoleEntry};
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// Visible-cell width of `s`, ignoring ANSI SGR escapes. ASCII-only
    /// approximation — matches the role picker's text content.
    fn visible_width(s: &str) -> usize {
        let mut count = 0usize;
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                // Skip CSI ... letter
                chars.next();
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            count += 1;
        }
        count
    }

    fn strip_ansi(s: &str) -> String {
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

    fn snapshot_scan() -> detect::ProjectScan {
        detect::ProjectScan {
            root: PathBuf::from("/repo/codeRoom"),
            stack: vec![
                detect::StackSignal::CargoToml,
                detect::StackSignal::GithubWorkflows,
                detect::StackSignal::ExistingClaudeMd { line_count: 42 },
            ],
            suggested_roles: vec!["host", "backend", "security", "ci"],
        }
    }

    fn snapshot_plan() -> Vec<RolePlan> {
        vec![
            RolePlan {
                name: "host".into(),
                engine: Engine::Cc,
            },
            RolePlan {
                name: "backend".into(),
                engine: Engine::Cc,
            },
            RolePlan {
                name: "security".into(),
                engine: Engine::Codex,
            },
        ]
    }

    fn sample_choices() -> Vec<RoleChoice> {
        ROLE_CATALOG
            .iter()
            .map(|info| RoleChoice {
                info: *info,
                selected: matches!(info.name, "host" | "backend" | "security"),
            })
            .collect()
    }

    #[test]
    fn snapshot_init_role_picker() {
        let scan = snapshot_scan();
        let rendered = strip_ansi(&render_role_picker(
            Path::new("/repo/codeRoom"),
            &scan,
            &sample_choices(),
            1,
        ));
        insta::assert_snapshot!(rendered, @r"
pick roles · setting up coderoom in codeRoom
space toggles · ↑↓ moves · enter continues · esc backs out

detected: Cargo.toml (Rust) · .github/workflows/ · CLAUDE.md (42 lines)

    [x] ● @host        orchestrates requests and keeps the room coherent · req…
  > [x] ● @backend     APIs, services, storage boundaries
    [ ] ● @frontend    UI, components, routing, client-side state
    [x] ● @security    authn, authz, threat modeling
    [ ] ● @data        schemas, migrations, query patterns
    [ ] ● @devops      CI/CD, infra, deploys, runtime health
    [ ] ● @ci          workflows, checks, release gates
    [ ] ● @qa          test strategy, edge cases, regression risk
    [ ] ● @docs        technical writing, examples, API reference

3 selected · host is always present · enter continues
");
    }

    #[test]
    fn snapshot_init_role_expansion_picker() {
        let scan = snapshot_scan();
        let rendered = strip_ansi(&render_role_expansion_picker(
            Path::new("/repo/codeRoom"),
            &scan,
            &sample_choices(),
            2,
        ));
        insta::assert_snapshot!(rendered, @r"
suggest roles · setting up coderoom in codeRoom
space toggles · ↑↓ moves · enter adds selected · esc skips

detected: Cargo.toml (Rust) · .github/workflows/ · CLAUDE.md (42 lines)
CodeRoom found only @host. Choose the specialists to add:

    [x] ● @host        orchestrates requests and keeps the room coherent · exi…
    [x] ● @backend     APIs, services, storage boundaries
  > [ ] ● @frontend    UI, components, routing, client-side state
    [x] ● @security    authn, authz, threat modeling
    [ ] ● @data        schemas, migrations, query patterns
    [ ] ● @devops      CI/CD, infra, deploys, runtime health
    [ ] ● @ci          workflows, checks, release gates
    [ ] ● @qa          test strategy, edge cases, regression risk
    [ ] ● @docs        technical writing, examples, API reference

2 new role(s) selected · enter writes config and priors
");
    }

    #[test]
    fn snapshot_init_engine_picker() {
        let installed = InstalledEngines {
            cc: true,
            codex: true,
            gemini: false,
        };
        let roles = vec!["host".into(), "backend".into(), "security".into()];
        let assignments = HashMap::from([
            ("host".into(), Engine::Cc),
            ("backend".into(), Engine::Cc),
            ("security".into(), Engine::Codex),
        ]);
        let rendered = strip_ansi(&render_engine_picker(
            Path::new("/repo/codeRoom"),
            &installed,
            &roles,
            &assignments,
            2,
        ));
        insta::assert_snapshot!(rendered, @r"
assign engines · setting up coderoom in codeRoom
↑/↓ moves · ←/→ cycles engine · enter continues · esc goes back

detected on your system:
  ✓ claude-code   installed
  ✓ codex         installed
  ✗ gemini-cli    not installed · github.com/google/gemini-cli

  role          ‹ engine     › model              note
    @host         ‹ claude-code › claude default     ready
    @backend      ‹ claude-code › claude default     ready
  > @security     ‹ codex      › codex default      ready

defaults are editable later in .coderoom/config.toml
");
    }

    #[test]
    fn snapshot_init_confirm() {
        let scan = snapshot_scan();
        let rendered = strip_ansi(&render_confirm(
            Path::new("/repo/codeRoom"),
            &scan,
            &snapshot_plan(),
        ));
        insta::assert_snapshot!(rendered, @r"
ready to write · setting up coderoom in codeRoom
nothing is written until Enter

will create:

.coderoom/
├─ config.toml              3 roles
├─ shared.md                project-wide priors
├─ roles/
│  ├─ host.md            claude-code
│  ├─ backend.md         claude-code
│  └─ security.md        codex
└─ .gitignore

  role           engine       focus
  @host          claude-code  orchestrates requests and keeps the room coherent
  @backend       claude-code  APIs, services, storage boundaries
  @security      codex        authn, authz, threat modeling

! found existing CLAUDE.md (42 lines).
  coderoom will not touch it; split assistance can land separately.

enter writes · esc goes back · q aborts
");
    }

    #[test]
    fn picker_row_never_exceeds_terminal_columns_at_60() {
        let info = role_info("backend");
        let row = picker_row(&info, true, true, 60, None);
        assert!(
            visible_width(&row) <= 60,
            "row visible width = {}, columns = 60, row = {row:?}",
            visible_width(&row)
        );
    }

    #[test]
    fn picker_row_never_exceeds_terminal_columns_at_80() {
        for info in ROLE_CATALOG {
            for selected in [true, false] {
                for is_cursor in [true, false] {
                    for tag in [None, Some("required"), Some("existing")] {
                        let row = picker_row(info, selected, is_cursor, 80, tag);
                        assert!(
                            visible_width(&row) <= 80,
                            "row visible width = {}, columns = 80, row = {row:?}",
                            visible_width(&row)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn picker_row_uses_more_room_at_120() {
        // At wider widths the description should not be truncated for
        // any of the catalog entries (their descriptions all fit).
        for info in ROLE_CATALOG {
            let row = picker_row(info, true, false, 120, None);
            assert!(
                !row.contains('…'),
                "120-col row should not be truncated, got {row:?}",
            );
        }
    }

    #[test]
    fn picker_row_handles_extreme_narrow_columns_without_panic() {
        // Below the floor (40 effective) we still produce output; the
        // description is heavily truncated but the row stays one line.
        let info = role_info("frontend");
        let _row = picker_row(&info, true, false, 30, None);
        let _row = picker_row(&info, true, false, 0, None);
    }

    /// Visual smoke. Run with:
    ///   cargo test --lib picker_visual_smoke -- --nocapture --ignored
    /// and eyeball the three rendered widths. Not a real test — it's a
    /// substitute for "open three terminals at 60/80/120 cols and try".
    #[test]
    #[ignore = "visual-only; render a sample picker at 60/80/120 cols for human review"]
    fn picker_visual_smoke() {
        for width in [60usize, 80, 120] {
            eprintln!("\n──── picker at columns = {width} ────");
            for (i, info) in ROLE_CATALOG.iter().enumerate() {
                let selected = matches!(info.name, "host" | "backend" | "security");
                let is_cursor = i == 1;
                let tag = if info.name == "host" {
                    Some("existing")
                } else {
                    None
                };
                eprintln!("{}", picker_row(info, selected, is_cursor, width, tag));
            }
        }
    }

    #[test]
    fn full_role_expansion_picker_fits_at_80_columns() {
        let dir = TempDir::new().unwrap();
        let scan = detect::scan(dir.path());
        let choices = sample_choices();
        // We can't override picker_columns() at the call site, so render
        // a single row at columns = 80 across the catalog and verify
        // none would exceed terminal width — the assemblage of rows in
        // render_role_expansion_picker shares the same width budget.
        for choice in &choices {
            let row = picker_row(&choice.info, choice.selected, false, 80, None);
            assert!(visible_width(&row) <= 80);
        }
        // Header / scan / footer lines come from push_header etc. They
        // are short by construction; only the rows hit the width gate.
        let _ = scan; // kept to anchor the project-scan codepath
    }

    fn host_only_config(default_engine: Engine, default_model: Option<&str>) -> Config {
        Config {
            default_engine,
            default_model: default_model.map(ToOwned::to_owned),
            budget_per_role_usd: 0.50,
            host_role: "host".into(),
            roles: HashMap::from([("host".into(), RoleEntry::default())]),
        }
    }

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
    fn default_host_only_config_is_expandable() {
        let cfg = host_only_config(Engine::Cc, None);
        assert!(is_default_host_only(&cfg));

        let mut cfg_with_backend = cfg.clone();
        cfg_with_backend
            .roles
            .insert("backend".into(), RoleEntry::default());
        assert!(!is_default_host_only(&cfg_with_backend));
    }

    #[test]
    fn expansion_defaults_keep_model_engine_pair_safe() {
        let cfg = host_only_config(Engine::Cc, Some("opus"));
        let installed = InstalledEngines {
            cc: true,
            codex: true,
            gemini: false,
        };

        assert_eq!(
            expansion_engine_for_role("security", &cfg, &installed),
            Engine::Cc
        );
        let additions = role_additions_from_plan(
            &[RolePlan {
                name: "security".into(),
                engine: Engine::Codex,
            }],
            &cfg,
        );
        assert_eq!(additions[0].engine, None);
    }

    #[test]
    fn expansion_uses_codex_for_security_when_no_default_model_can_leak() {
        let cfg = host_only_config(Engine::Cc, None);
        let installed = InstalledEngines {
            cc: true,
            codex: true,
            gemini: false,
        };
        let engine = expansion_engine_for_role("security", &cfg, &installed);
        assert_eq!(engine, Engine::Codex);

        let additions = role_additions_from_plan(
            &[RolePlan {
                name: "security".into(),
                engine,
            }],
            &cfg,
        );
        assert_eq!(additions[0].engine, Some(Engine::Codex));
        assert_eq!(additions[0].model, None);
    }

    #[test]
    fn default_priors_templates_stay_compact() {
        assert!(word_count(DEFAULT_HOST_PRIORS) <= 160);
        assert!(word_count(DEFAULT_ROLE_TEMPLATE) <= 180);
        assert!(word_count(DEFAULT_SHARED_PRIORS) <= 220);
        for required in ["CodeRoom", "@name", "From @", "/patch", "/journal"] {
            assert!(
                DEFAULT_SHARED_PRIORS.contains(required),
                "shared priors should explain {required}"
            );
        }
        for required in ["@host", "specialist", "From @role"] {
            assert!(
                DEFAULT_HOST_PRIORS.contains(required),
                "host priors should explain {required}"
            );
        }
        for required in ["{ROLE}", "{HOST}", "{PEERS}", "From @role"] {
            assert!(
                DEFAULT_ROLE_TEMPLATE.contains(required),
                "role template should contain {required}"
            );
        }
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
        assert!(!backend_priors.contains("{HOST}"));
        assert!(!backend_priors.contains("{PEERS}"));
        assert!(backend_priors.contains("@host"));
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

    fn word_count(input: &str) -> usize {
        input.split_whitespace().count()
    }
}
