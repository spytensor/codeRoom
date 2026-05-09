//! `cr` — the CodeRoom CLI binary.
//!
//! Subcommands at v0.1:
//!
//! - `cr start [--project PATH]` — enter the interactive REPL.
//!
//! Future subcommands (`cr init`, `cr role`, `cr show`, `cr cost`) land in
//! their own PRs per the v0.1 sequence in `docs/architecture.md`.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

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
    /// Enter the interactive REPL using `.coderoom/config.toml` in the
    /// current directory (or `--project`).
    Start {
        /// Project root containing `.coderoom/`. Defaults to the current
        /// working directory.
        #[arg(long)]
        project: Option<PathBuf>,
    },
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

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        match cli.command {
            None => {
                println!(
                    "cr {} — pre-alpha. Run `cr start` (and see `cr --help`).",
                    env!("CARGO_PKG_VERSION")
                );
                Ok(())
            }
            Some(Cmd::Start { project }) => {
                let project_root = match project {
                    Some(p) => p,
                    None => std::env::current_dir()?,
                };
                coderoom::repl::run(&project_root).await
            }
        }
    })
}
