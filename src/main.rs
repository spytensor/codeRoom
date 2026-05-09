//! `cr` — the CodeRoom CLI binary.
//!
//! At v0.1 bootstrap this is a placeholder that reports its version. Each
//! subcommand (`init`, `role`, `start`, `show`, `cost`) is added in a
//! dedicated PR following the architecture constitution in
//! `docs/architecture.md`.

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "cr",
    version,
    about = "CodeRoom — coordination shell for multi-role agent CLI sessions",
    long_about = None,
)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
    println!(
        "cr {} — bootstrap build (no subcommands yet)",
        env!("CARGO_PKG_VERSION")
    );
}
