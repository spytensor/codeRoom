//! `cr` — the CodeRoom CLI binary.
//!
//! Subcommands at v0.1:
//!
//! - `cr init [--project PATH]`  — bootstrap `.coderoom/` in a fresh project
//! - `cr role add <name> [--engine cc|codex|gemini] [--model X]` — add a role
//! - `cr role list`              — list configured roles
//! - `cr role rm <name>`         — remove a role (refuses for the host)
//! - `cr start [--project PATH]` — enter the interactive REPL
//!
//! Future subcommands (`cr show`, `cr cost`) land in their own PRs per the
//! v0.1 sequence in `docs/architecture.md`.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use coderoom::adapter::Engine;

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
    /// Bootstrap a `.coderoom/` directory with a default `@host` role.
    Init {
        /// Project root in which to create `.coderoom/`. Defaults to the
        /// current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
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
    },
    /// Replay `.coderoom/messages.jsonl` through the live renderer.
    Show {
        /// Project root. Defaults to the current working directory.
        #[arg(long)]
        project: Option<PathBuf>,
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

fn project_root_or_cwd(arg: Option<PathBuf>) -> std::io::Result<PathBuf> {
    match arg {
        Some(p) => Ok(p),
        None => std::env::current_dir(),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    match cli.command {
        None => {
            println!(
                "cr {} — try `cr init` then `cr start`. See `cr --help`.",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        Some(Cmd::Init { project }) => coderoom::init::run(&project_root_or_cwd(project)?),
        Some(Cmd::Role { command }) => run_role_cmd(command),
        Some(Cmd::Start { project }) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                let project_root = project_root_or_cwd(project)?;
                coderoom::repl::run(&project_root).await
            })
        }
        Some(Cmd::Show { project }) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                let project_root = project_root_or_cwd(project)?;
                coderoom::repl::show_log(&project_root).await
            })
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
    }
}
