//! `cr init` — bootstrap a project's `.coderoom/` directory.
//!
//! v0.1 init is non-interactive and idempotent-by-default-refusing:
//! it creates a `.coderoom/` skeleton with one default `@host` role and
//! refuses to run if the directory already exists. Users who want a
//! richer setup edit the generated files (or, in a later PR, run
//! `cr role add`).

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::config::{CODEROOM_DIR, CONFIG_FILE, ROLES_DIR};

const DEFAULT_CONFIG_TOML: &str = include_str!("init_defaults/config.toml");
const DEFAULT_HOST_PRIORS: &str = include_str!("init_defaults/host.md");
const DEFAULT_SHARED_PRIORS: &str = include_str!("init_defaults/shared.md");
const DEFAULT_GITIGNORE: &str = include_str!("init_defaults/gitignore");

/// Initialize a project's `.coderoom/` directory at `project_root`.
///
/// Returns `Err` if `.coderoom/` already exists at the target path.
pub fn run(project_root: &Path) -> Result<()> {
    let coderoom_dir = project_root.join(CODEROOM_DIR);
    if coderoom_dir.exists() {
        bail!(
            "{} already exists — refusing to overwrite. Edit it manually or remove it first.",
            coderoom_dir.display(),
        );
    }

    let roles_dir = coderoom_dir.join(ROLES_DIR);
    std::fs::create_dir_all(&roles_dir)
        .with_context(|| format!("creating {}", roles_dir.display()))?;

    write_file(&coderoom_dir.join(CONFIG_FILE), DEFAULT_CONFIG_TOML)?;
    write_file(&coderoom_dir.join("shared.md"), DEFAULT_SHARED_PRIORS)?;
    write_file(&roles_dir.join("host.md"), DEFAULT_HOST_PRIORS)?;
    write_file(&coderoom_dir.join(".gitignore"), DEFAULT_GITIGNORE)?;

    println!("Initialized {}", coderoom_dir.display());
    println!();
    println!("  - {CONFIG_FILE}: project config (default_engine = cc, host_role = host)");
    println!("  - {ROLES_DIR}/host.md: priors for the @host role (edit me!)");
    println!("  - shared.md: priors loaded by every role");
    println!("  - .gitignore: excludes runtime artifacts (transcripts, sessions, messages.jsonl)");
    println!();
    println!("Next:");
    println!("  1. Edit .coderoom/roles/host.md to give @host real project priors.");
    println!("  2. Add more roles by creating .coderoom/roles/<name>.md and");
    println!("     declaring them under [roles.<name>] in config.toml.");
    println!("  3. Run `cr start`.");

    Ok(())
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[test]
    fn init_creates_minimal_valid_layout() {
        let tmp = TempDir::new().unwrap();
        run(tmp.path()).expect("init succeeds in fresh dir");

        let coderoom = tmp.path().join(CODEROOM_DIR);
        assert!(coderoom.is_dir());
        assert!(coderoom.join(CONFIG_FILE).is_file());
        assert!(coderoom.join("shared.md").is_file());
        assert!(coderoom.join(ROLES_DIR).join("host.md").is_file());
        assert!(coderoom.join(".gitignore").is_file());
    }

    #[test]
    fn init_output_passes_config_validation() {
        let tmp = TempDir::new().unwrap();
        run(tmp.path()).expect("init");
        let cfg = Config::load(tmp.path()).expect("init output should be a valid config");
        assert_eq!(cfg.host_role, "host");
        assert!(cfg.is_host("host"));
    }

    #[test]
    fn init_refuses_to_overwrite_existing_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(CODEROOM_DIR)).unwrap();
        let err = run(tmp.path()).expect_err("should refuse to overwrite");
        assert!(err.to_string().contains("already exists"), "got: {err}");
    }
}
