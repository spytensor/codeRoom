//! `cr` — the CodeRoom CLI binary.
//!
//! Subcommands at v0.1:
//!
//! - `cr init [--project PATH]`  — bootstrap `.coderoom/` in a fresh project
//! - `cr role add <name> [--engine cc|codex|gemini] [--model X]` — add a role
//! - `cr role list`              — list configured roles
//! - `cr role rm <name>`         — remove a role (refuses for the host)
//! - `cr [start] [--project PATH]` — enter the interactive REPL
//! - `cr prompt show <role>`     — print a role's effective prompt
//! - `cr doctor [--fix]`         — inspect CodeRoom project files
//! - `cr show [--role ROLE] [--since YYYY-MM-DD] [--tail N]` — replay events
//! - `cr cost [--since YYYY-MM-DD]` — summarize reported engine spend

use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use coderoom::adapter::{Engine, PermissionMode};
use coderoom::config_cmd::LayerTarget;

#[derive(Debug, Parser)]
#[command(
    name = "cr",
    version,
    about = "CodeRoom — coordination shell for multi-role agent CLI sessions",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Bootstrap a `.coderoom/` directory with detected default roles.
    Init {
        /// Project root in which to create `.coderoom/`. Defaults to the
        /// current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Skip the `proceed?` prompt and accept all defaults.
        /// For dotfile repos / onboarding scripts.
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
    /// Manage roles in the current project's `.coderoom/config.toml`.
    Role {
        #[command(subcommand)]
        command: RoleCmd,
    },
    /// Enter the interactive REPL using `.coderoom/config.toml` in the
    /// current directory (or `--project`).
    Start {
        /// Project root containing `.coderoom/`. Defaults to the current
        /// working directory.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Run this session with permission_mode=bypass for every role.
        #[arg(long)]
        yolo: bool,
        /// Start every role with a fresh engine session instead of
        /// resuming the prior conversation. Default behaviour (per
        /// amendment A-006) is to resume from
        /// `.coderoom/sessions/ids/<role>.id` when present; pass
        /// `--fresh` to clear those ids and start clean. The user-
        /// facing equivalent of `claude --resume` vs no flag.
        #[arg(long)]
        fresh: bool,
    },
    /// Replay `.coderoom/messages.jsonl` through the live renderer.
    Show {
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Only replay events for this role. A leading `@` is accepted.
        #[arg(long)]
        role: Option<String>,
        /// Skip the log entirely if its mtime is older than this date
        /// (`YYYY-MM-DD`). v0.1 limitation — proper per-event timestamps
        /// land in v0.2.
        #[arg(long, value_parser = parse_date)]
        since: Option<chrono::NaiveDate>,
        /// Render only the last N matching events.
        #[arg(long)]
        tail: Option<usize>,
    },
    /// Per-role cost summary aggregated from `.coderoom/messages.jsonl`.
    Cost {
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Skip the log entirely if its mtime is older than this date
        /// (`YYYY-MM-DD`). v0.1 limitation — proper per-event timestamps
        /// land in v0.2.
        #[arg(long, value_parser = parse_date)]
        since: Option<chrono::NaiveDate>,
    },
    /// Compact archived patches and old journals into a role's priors.
    Compact {
        /// Role name to compact.
        role: String,
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// List every git-backed pointer the role's priors reference, with
    /// resolution status (fresh / stale / unresolvable).
    #[command(long_about = "\
List every `[[…]]` pointer in the role's priors file with its current \
resolution status. Useful for spotting which anchors fell behind HEAD \
before re-prompting.

Token grammar (write these inside a role's .md file):

  [[<path>#L<n>-<m>@<sha>]]   locked to a commit, line range
  [[<path>#L<n>@<sha>]]        locked single line
  [[<path>@<sha>]]              locked whole file
  [[<path>#L<n>-<m>]]           HEAD range
  [[<path>@HEAD]]                HEAD whole file (explicit)

Every pointer must carry at least one anchor — `#L<range>` or `@<sha>` / \
`@HEAD`. Unanchored `[[bare-word]]` tokens are intentionally rejected at \
parse time so prose like `[[TODO]]` doesn't accidentally trigger a file \
read. The HEAD-tracking branch is also containment-checked: any path \
that canonicalises outside the repo root is refused.

When a pointer's locked sha falls behind HEAD, the priors render flags it \
as stale and keeps the content from the original sha. Update the sha or \
switch to `@HEAD` when you've reviewed the new content.")]
    Pointers {
        /// Role name. Leading `@` is accepted.
        role: String,
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Inspect or edit the layered config (user / project / .local).
    Config {
        #[command(subcommand)]
        command: ConfigCmd,
    },
    /// Inspect composed role prompts.
    Prompt {
        #[command(subcommand)]
        command: PromptCmd,
    },
    /// Diagnose CodeRoom project files.
    Doctor {
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Apply exact safe fixes.
        #[arg(long)]
        fix: bool,
    },
    /// Check the npm registry for a newer `cr` and report the diff.
    /// Read-only — does not touch the installed binary. Run
    /// `cr upgrade` to actually install.
    Update,
    /// Upgrade the `cr` binary in place via whichever method
    /// originally installed it (currently only `npm install -g` is
    /// auto-upgradable; other paths print instructions). Verifies
    /// the binary on disk actually changed before claiming success.
    Upgrade,
    /// Internal Claude Code hook entry point.
    #[command(name = "__coderoom-hook-decision", hide = true)]
    HookDecision {
        /// Permission mode to apply to this hook decision.
        #[arg(long, value_parser = parse_permission_mode)]
        mode: PermissionMode,
        /// Session policy file populated by `/allow` and `/deny`.
        #[arg(long)]
        policy_file: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum RoleCmd {
    /// Add a new role.
    Add {
        /// Role name (ASCII letters/digits/`-`/`_`, must start with a letter).
        name: String,
        /// Override default engine for this role.
        #[arg(long, value_parser = parse_engine)]
        engine: Option<Engine>,
        /// Override default model for this role.
        #[arg(long)]
        model: Option<String>,
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// List configured roles.
    List {
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Remove a role (refuses for the configured host).
    Rm {
        /// Role name to remove.
        name: String,
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Promote an existing role to host in project config.
    Host {
        /// Role name to make host.
        name: String,
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCmd {
    /// Print the effective merged config plus which layer files were
    /// read. Use this to debug "why is my engine cc when I set codex
    /// in user config?" — answer is in the layer footer.
    Show {
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Open `$EDITOR` (or `$VISUAL`) on a layer's config file.
    /// Creates a commented stub for `--user` / `--local` if missing;
    /// refuses `--project` if `.coderoom/config.toml` is missing
    /// (run `cr init` first).
    Edit {
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Edit the user-level config (~/.config/coderoom/config.toml).
        #[arg(long, group = "layer")]
        user: bool,
        /// Edit the project-local override (.coderoom/config.local.toml).
        #[arg(long, group = "layer")]
        local: bool,
    },
    /// Print the absolute path of a layer's config file.
    Path {
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Print the user-level path.
        #[arg(long, group = "layer")]
        user: bool,
        /// Print the project-local path.
        #[arg(long, group = "layer")]
        local: bool,
    },
    /// Print one effective config value, such as `default_engine`.
    Get {
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Dotted key to read.
        key: String,
    },
    /// Set one config value. Defaults to the user layer; pass
    /// `--project-layer` or `--local` for other writable layers.
    Set {
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Write project `.coderoom/config.toml`.
        #[arg(long = "project-layer", group = "layer")]
        project_layer: bool,
        /// Write the project-local override.
        #[arg(long, group = "layer")]
        local: bool,
        /// Dotted key to set.
        key: String,
        /// TOML-ish scalar value to write.
        value: String,
    },
}

#[derive(Debug, Subcommand)]
enum PromptCmd {
    /// Print the effective prompt for one role.
    Show {
        /// Role name. A leading `@` is accepted.
        role: String,
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
    },
}

fn layer_from_flags(user: bool, local: bool) -> LayerTarget {
    match (user, local) {
        (true, _) => LayerTarget::User,
        (_, true) => LayerTarget::Local,
        // Default: project. clap's `group = "layer"` already makes
        // --user / --local mutually exclusive at parse time.
        _ => LayerTarget::Project,
    }
}

fn parse_engine(s: &str) -> Result<Engine, String> {
    match s {
        "cc" => Ok(Engine::Cc),
        "codex" => Ok(Engine::Codex),
        "gemini" => Ok(Engine::Gemini),
        other => Err(format!(
            "unknown engine `{other}` — valid: cc, codex, gemini"
        )),
    }
}

fn parse_permission_mode(s: &str) -> Result<PermissionMode, String> {
    match s {
        "ask" => Ok(PermissionMode::Ask),
        "auto" => Ok(PermissionMode::Auto),
        "bypass" => Ok(PermissionMode::Bypass),
        other => Err(format!(
            "unknown permission mode `{other}` — valid: ask, auto, bypass"
        )),
    }
}

fn parse_date(s: &str) -> std::result::Result<chrono::NaiveDate, String> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|e| format!("must be YYYY-MM-DD: {e}"))
}

fn project_root_or_cwd(arg: Option<PathBuf>) -> std::io::Result<PathBuf> {
    match arg {
        Some(p) => Ok(p),
        None => std::env::current_dir(),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(Cmd::HookDecision { mode, policy_file }) = &cli.command {
        return coderoom::permissions::run_claude_hook(*mode, policy_file.as_deref());
    }

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();
    coderoom::output::print_terminal_probe();

    // Engine-binary check up front. `cr config`, `cr update`, and
    // `cr upgrade` are useful without any engine installed (inspecting
    // or fixing the very setup that's missing); everything else
    // requires at least one of claude / codex / gemini on $PATH.
    let needs_engine = !matches!(
        cli.command,
        Some(
            Cmd::Config { .. }
                | Cmd::Prompt { .. }
                | Cmd::Doctor { .. }
                | Cmd::Update
                | Cmd::Upgrade
                | Cmd::HookDecision { .. }
        )
    );
    if needs_engine && coderoom::engines::require_any_installed().is_err() {
        std::process::exit(1);
    }

    match cli.command {
        None => run_start(None, false, false),
        Some(Cmd::Init { project, yes }) => {
            let opts = if yes {
                coderoom::init::InitOptions::accepted_defaults()
            } else {
                coderoom::init::InitOptions::manual()
            };
            coderoom::init::run(&project_root_or_cwd(project)?, opts)
        }
        Some(Cmd::Role { command }) => run_role_cmd(command),
        Some(Cmd::Start {
            project,
            yolo,
            fresh,
        }) => run_start(project, yolo, fresh),
        Some(Cmd::Show {
            project,
            role,
            since,
            tail,
        }) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                let project_root = project_root_or_cwd(project)?;
                let options = coderoom::repl::ShowOptions {
                    role: role.map(|role| role.strip_prefix('@').unwrap_or(&role).to_owned()),
                    since,
                    tail,
                };
                coderoom::repl::show_log(&project_root, &options).await
            })
        }
        Some(Cmd::Config { command }) => run_config_cmd(command),
        Some(Cmd::Prompt { command }) => run_prompt_cmd(command),
        Some(Cmd::Doctor { project, fix }) => {
            let root = project_root_or_cwd(project)?;
            coderoom::doctor::run(&root, coderoom::doctor::DoctorOptions { fix })
        }
        Some(Cmd::Update) => coderoom::update::check(),
        Some(Cmd::Upgrade) => coderoom::update::upgrade(),
        Some(Cmd::HookDecision { .. }) => unreachable!("handled before terminal setup"),
        Some(Cmd::Compact { role, project }) => {
            let root = project_root_or_cwd(project)?;
            let role = role.strip_prefix('@').unwrap_or(&role);
            let path =
                coderoom::priors::compact_role(&root.join(coderoom::config::CODEROOM_DIR), role)?;
            println!("compacted @{role} history into {}", path.display());
            Ok(())
        }
        Some(Cmd::Pointers { role, project }) => {
            let root = project_root_or_cwd(project)?;
            let role = role.strip_prefix('@').unwrap_or(&role);
            run_pointers(&root, role)
        }
        Some(Cmd::Cost { project, since }) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                let project_root = project_root_or_cwd(project)?;
                coderoom::cost::run(&project_root, since).await
            })
        }
    }
}

fn run_prompt_cmd(cmd: PromptCmd) -> Result<()> {
    match cmd {
        PromptCmd::Show { role, project } => {
            coderoom::prompt_cmd::show(&project_root_or_cwd(project)?, &role)
        }
    }
}

/// `cr pointers @<role>` — read the role's priors file and list each
/// `[[…]]` pointer with its resolution status. Lives here (not in the
/// `pointers` library module) so the module stays pure data-in /
/// data-out with no `crate::config` or `println!` dependencies; the
/// future Contracts / Inbox layers reuse the library half.
fn run_pointers(project_root: &Path, role: &str) -> Result<()> {
    use coderoom::config::{CODEROOM_DIR, ROLES_DIR};
    use coderoom::pointers::{
        resolve_all, status_glyph, status_word, Pointer, PointerStatus, UnresolvableReason,
    };

    let priors_path = project_root
        .join(CODEROOM_DIR)
        .join(ROLES_DIR)
        .join(format!("{role}.md"));
    let priors = std::fs::read_to_string(&priors_path).map_err(|e| {
        anyhow::anyhow!(
            "could not read priors for @{role} at {}: {e} \
             (run `cr role list` to see existing roles)",
            priors_path.display()
        )
    })?;

    let resolved = resolve_all(&priors, project_root);
    if resolved.is_empty() {
        println!(
            "@{role} has no pointers in its priors file. \
             Add one with `[[<path>#L<n>-<m>@<sha>]]` or `[[<path>@HEAD]]`.\n\
             See `cr pointers --help` for the full grammar."
        );
        return Ok(());
    }
    println!("pointers in @{role} priors:");
    for r in &resolved {
        // Short locked SHA matches the short HEAD form used elsewhere,
        // so the line doesn't wrap on 80-col terminals and the two
        // SHAs are visually comparable.
        let display_pointer = Pointer {
            path: r.pointer.path.clone(),
            line_range: r.pointer.line_range,
            locked_sha: r
                .pointer
                .locked_sha
                .as_ref()
                .map(|s| s.chars().take(8).collect::<String>()),
        };
        let status_extra = match &r.status {
            PointerStatus::Fresh => String::new(),
            PointerStatus::Stale { head_sha } => format!(" (HEAD at {head_sha})"),
            PointerStatus::Unresolvable(reason) => match reason {
                UnresolvableReason::ShaNotFound { .. } => " (sha gone)".to_owned(),
                UnresolvableReason::NotAGitRepo { .. } => " (not a git repo)".to_owned(),
                UnresolvableReason::PathEscapesRepo { .. } => {
                    " (path escapes repo — security gate)".to_owned()
                }
                UnresolvableReason::PathNotFoundAtSha { .. } => " (path missing at sha)".to_owned(),
                _ => String::new(),
            },
        };
        println!(
            "  {} [[{display_pointer}]]  [{}{status_extra}]",
            status_glyph(&r.status),
            status_word(&r.status),
        );
        // For unresolvable pointers, print the actionable reason on a
        // second indented line so the user sees the remediation hint
        // without having to dig.
        if let PointerStatus::Unresolvable(reason) = &r.status {
            println!("      → {reason}");
        }
    }
    Ok(())
}

fn run_start(project: Option<PathBuf>, yolo: bool, fresh: bool) -> Result<()> {
    if yolo && !confirm_yolo()? {
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let project_root = project_root_or_cwd(project)?;
        let options = coderoom::repl::RunOptions {
            permission_mode_override: yolo.then_some(PermissionMode::Bypass),
            fresh,
        };
        coderoom::repl::run_with_options(&project_root, options).await
    })
}

fn confirm_yolo() -> Result<bool> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(true);
    }
    print!("Run this CodeRoom session with permission_mode=bypass for every role? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

fn run_config_cmd(cmd: ConfigCmd) -> Result<()> {
    match cmd {
        ConfigCmd::Show { project } => coderoom::config_cmd::show(&project_root_or_cwd(project)?),
        ConfigCmd::Edit {
            project,
            user,
            local,
        } => {
            let layer = layer_from_flags(user, local);
            coderoom::config_cmd::edit(layer, &project_root_or_cwd(project)?)
        }
        ConfigCmd::Path {
            project,
            user,
            local,
        } => {
            let layer = layer_from_flags(user, local);
            coderoom::config_cmd::path(layer, &project_root_or_cwd(project)?)
        }
        ConfigCmd::Get { project, key } => {
            coderoom::config_cmd::get(&project_root_or_cwd(project)?, &key)
        }
        ConfigCmd::Set {
            project,
            project_layer,
            local,
            key,
            value,
        } => {
            let layer = if project_layer {
                LayerTarget::Project
            } else if local {
                LayerTarget::Local
            } else {
                LayerTarget::User
            };
            coderoom::config_cmd::set(layer, &project_root_or_cwd(project)?, &key, &value)
        }
    }
}

fn run_role_cmd(cmd: RoleCmd) -> Result<()> {
    match cmd {
        RoleCmd::Add {
            name,
            engine,
            model,
            project,
        } => {
            let root = project_root_or_cwd(project)?;
            coderoom::role::add(&root, &name, engine, model.as_deref())
        }
        RoleCmd::List { project } => coderoom::role::list(&project_root_or_cwd(project)?),
        RoleCmd::Rm { name, project } => coderoom::role::rm(&project_root_or_cwd(project)?, &name),
        RoleCmd::Host { name, project } => {
            coderoom::role::set_host(&project_root_or_cwd(project)?, &name)
        }
    }
}
