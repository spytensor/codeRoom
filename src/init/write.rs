use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::adapter::Engine;
use crate::config::{CONFIG_FILE, ROLES_DIR};

use super::{
    RolePlan, DEFAULT_ENGINE, DEFAULT_GITIGNORE, DEFAULT_HOST_PRIORS, DEFAULT_ROLE_TEMPLATE,
    DEFAULT_SHARED_PRIORS,
};

/// Materialize the `.coderoom/` skeleton on disk. Each role gets a
/// templated priors file with `{ROLE}` substituted.
pub(super) fn write_all(coderoom_dir: &Path, roles: &[RolePlan]) -> Result<()> {
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
        "# Default permission mode for tools: ask | auto | bypass."
    );
    let _ = writeln!(out, "permission_mode = \"ask\"");
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
        match role.engine {
            Engine::Codex => {
                let _ = writeln!(
                    out,
                    "# Codex can use ask/auto only in a live REPL; generated roles default to bypass."
                );
                let _ = writeln!(out, "permission_mode = \"bypass\"");
            }
            Engine::Gemini => {
                let _ = writeln!(
                    out,
                    "# Gemini is bypass-only until its approval bridge is supervised."
                );
                let _ = writeln!(out, "permission_mode = \"bypass\"");
            }
            Engine::Cc => {}
        }
        let _ = writeln!(out);
    }
    out
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
