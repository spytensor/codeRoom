//! `cr role add/list/rm` implementations.
//!
//! Each command mutates `.coderoom/config.toml` and/or
//! `.coderoom/roles/<name>.md` with the same validation discipline that
//! [`crate::config::Config::load`] enforces at REPL startup, so the
//! generated state is always loadable.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use crate::adapter::Engine;
use crate::config::{Config, CODEROOM_DIR, CONFIG_FILE, ROLES_DIR};

/// Default body for a freshly-scaffolded role priors file. Users are
/// expected to replace this with project-specific guidance.
const DEFAULT_ROLE_PRIORS: &str = include_str!("init_defaults/role_template.md");

/// Add a new role. Updates `config.toml` (inserts `[roles.<name>]` with
/// optional engine/model overrides), then creates an empty priors file
/// at `.coderoom/roles/<name>.md` if one doesn't already exist.
pub fn add(
    project_root: &Path,
    name: &str,
    engine: Option<Engine>,
    model: Option<&str>,
) -> Result<()> {
    validate_name(name)?;
    let coderoom_dir = project_root.join(CODEROOM_DIR);
    if !coderoom_dir.is_dir() {
        bail!("{} not found — run `cr init` first", coderoom_dir.display(),);
    }

    let mut cfg = read_config(&coderoom_dir)?;
    if cfg.roles.contains_key(name) {
        bail!("role `{name}` already exists in {CONFIG_FILE}");
    }

    let entry = crate::config::RoleEntry {
        engine,
        model: model.map(ToOwned::to_owned),
    };
    cfg.roles.insert(name.to_owned(), entry);
    write_config(&coderoom_dir, &cfg)?;

    let priors_path = coderoom_dir.join(ROLES_DIR).join(format!("{name}.md"));
    if !priors_path.exists() {
        std::fs::create_dir_all(priors_path.parent().expect("roles parent"))
            .with_context(|| format!("creating roles dir for `{name}`"))?;
        std::fs::write(&priors_path, render_role_template(name))
            .with_context(|| format!("writing {}", priors_path.display()))?;
    }

    println!("✓ added role @{name}");
    if let Some(engine) = engine {
        println!("  engine: {}", engine.as_str());
    }
    if let Some(model) = model {
        println!("  model:  {model}");
    }
    println!("  priors: {}", priors_path.display());
    println!();
    println!("  edit the priors file, then `cr start` (or `/refresh @{name}` if running)");

    Ok(())
}

/// Print the configured roles, one per line, with engine + host marker.
pub fn list(project_root: &Path) -> Result<()> {
    let coderoom_dir = project_root.join(CODEROOM_DIR);
    let cfg = read_config(&coderoom_dir)?;

    let mut names: Vec<&str> = cfg.role_names().collect();
    names.sort_unstable();
    if names.is_empty() {
        println!("(no roles configured — use `cr role add <name>`)");
        return Ok(());
    }
    for name in names {
        let entry = cfg.roles.get(name);
        let engine = entry
            .and_then(|e| e.engine)
            .unwrap_or(cfg.default_engine)
            .as_str();
        let model = entry
            .and_then(|e| e.model.as_deref())
            .or(cfg.default_model.as_deref())
            .unwrap_or("(default)");
        let host_marker = if cfg.is_host(name) { " (host)" } else { "" };
        println!("@{name:<14} engine={engine:<6} model={model}{host_marker}");
    }
    Ok(())
}

/// Remove a role. Refuses if it's the configured host. Removes both the
/// `[roles.<name>]` table and the priors file.
pub fn rm(project_root: &Path, name: &str) -> Result<()> {
    let coderoom_dir = project_root.join(CODEROOM_DIR);
    let mut cfg = read_config(&coderoom_dir)?;

    if !cfg.roles.contains_key(name) {
        bail!("no such role: @{name}");
    }
    if cfg.is_host(name) {
        bail!(
            "@{name} is the host role; change `host_role` in {CONFIG_FILE} first, \
             or use `cr role add` to introduce a replacement"
        );
    }

    cfg.roles.remove(name);
    write_config(&coderoom_dir, &cfg)?;

    let priors_path = coderoom_dir.join(ROLES_DIR).join(format!("{name}.md"));
    if priors_path.is_file() {
        std::fs::remove_file(&priors_path)
            .with_context(|| format!("removing {}", priors_path.display()))?;
    }

    println!("✓ removed @{name}");
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("role name must be non-empty");
    }
    if name.starts_with('@') {
        bail!("role name should not include the leading `@`");
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    if !name.chars().all(allowed) {
        bail!(
            "role name `{name}` contains invalid characters; use ASCII letters, digits, `-`, `_`"
        );
    }
    if !name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        bail!("role name must start with an ASCII letter");
    }
    Ok(())
}

fn read_config(coderoom_dir: &Path) -> Result<Config> {
    let path = coderoom_dir.join(CONFIG_FILE);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(cfg)
}

fn write_config(coderoom_dir: &Path, cfg: &Config) -> Result<()> {
    let path = coderoom_dir.join(CONFIG_FILE);
    let body = toml::to_string_pretty(cfg).map_err(|e| anyhow!("serializing config.toml: {e}"))?;
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn render_role_template(name: &str) -> String {
    DEFAULT_ROLE_PRIORS.replace("{ROLE}", name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::TempDir;

    /// Build a `.coderoom/` skeleton with one role (host) so the
    /// commands have a valid starting point.
    fn fixture() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
        fs::write(
            coderoom.join(CONFIG_FILE),
            r#"
default_engine = "cc"
budget_per_role_usd = 0.50
host_role = "host"

[roles.host]
"#,
        )
        .unwrap();
        fs::write(coderoom.join(ROLES_DIR).join("host.md"), "host priors").unwrap();
        tmp
    }

    #[test]
    fn add_creates_role_entry_and_priors_file() {
        let tmp = fixture();
        add(tmp.path(), "backend", None, None).unwrap();

        let coderoom = tmp.path().join(CODEROOM_DIR);
        let cfg = Config::load(tmp.path()).unwrap();
        assert!(cfg.roles.contains_key("backend"));
        assert!(coderoom.join(ROLES_DIR).join("backend.md").is_file());
    }

    #[test]
    fn add_persists_engine_and_model_overrides() {
        let tmp = fixture();
        add(tmp.path(), "security", Some(Engine::Codex), Some("o3")).unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        let entry = cfg.roles.get("security").unwrap();
        assert_eq!(entry.engine, Some(Engine::Codex));
        assert_eq!(entry.model.as_deref(), Some("o3"));
    }

    #[test]
    fn add_refuses_duplicate_role() {
        let tmp = fixture();
        add(tmp.path(), "backend", None, None).unwrap();
        let err = add(tmp.path(), "backend", None, None).expect_err("duplicate add");
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn add_validates_name() {
        let tmp = fixture();
        let err = add(tmp.path(), "@backend", None, None).expect_err("leading @");
        assert!(err.to_string().contains("@"));

        let err = add(tmp.path(), "1bad", None, None).expect_err("starts with digit");
        assert!(err.to_string().contains("ASCII letter"));

        let err = add(tmp.path(), "with spaces", None, None).expect_err("space");
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn rm_removes_role_and_priors() {
        let tmp = fixture();
        add(tmp.path(), "backend", None, None).unwrap();
        rm(tmp.path(), "backend").unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert!(!cfg.roles.contains_key("backend"));
        assert!(!tmp
            .path()
            .join(CODEROOM_DIR)
            .join(ROLES_DIR)
            .join("backend.md")
            .is_file());
    }

    #[test]
    fn rm_refuses_to_remove_host() {
        let tmp = fixture();
        let err = rm(tmp.path(), "host").expect_err("host should be protected");
        assert!(err.to_string().contains("host"));
    }

    #[test]
    fn rm_unknown_role_errors() {
        let tmp = fixture();
        let err = rm(tmp.path(), "ghost").expect_err("unknown role");
        assert!(err.to_string().contains("ghost"));
    }
}
