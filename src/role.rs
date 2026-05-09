//! `cr role add/list/rm` implementations.
//!
//! Each command mutates `.coderoom/config.toml` and/or
//! `.coderoom/roles/<name>.md` with the same validation discipline that
//! [`crate::config::Config::load`] enforces at REPL startup, so the
//! generated state is always loadable.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use crate::adapter::Engine;
use crate::config::{Config, RoleEntry, CODEROOM_DIR, CONFIG_FILE, ROLES_DIR};
use crate::config_layered::ProjectConfigRaw;

/// Default body for a freshly-scaffolded role priors file. Users are
/// expected to replace this with project-specific guidance.
const DEFAULT_ROLE_PRIORS: &str = include_str!("init_defaults/role_template.md");

/// One role to append to an existing `.coderoom/` project config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoleAddition {
    /// Role name without the leading `@`.
    pub(crate) name: String,
    /// Project-layer engine override. `None` inherits the effective
    /// default engine.
    pub(crate) engine: Option<Engine>,
    /// Project-layer model override. `None` inherits the effective
    /// model for that engine.
    pub(crate) model: Option<String>,
}

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

    let mut raw = read_project_raw(&coderoom_dir)?;
    if raw.roles.contains_key(name) {
        bail!("role `{name}` already exists in {CONFIG_FILE}");
    }

    let entry = RoleEntry {
        engine,
        model: model.map(ToOwned::to_owned),
    };
    raw.roles.insert(name.to_owned(), entry);
    write_project_raw(&coderoom_dir, &raw)?;

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

/// Add several roles in one config write. Priors files are created
/// before `config.toml` is updated, so a file-write failure cannot
/// leave config pointing at a missing role priors file.
pub(crate) fn add_many(project_root: &Path, additions: &[RoleAddition]) -> Result<usize> {
    let coderoom_dir = project_root.join(CODEROOM_DIR);
    if !coderoom_dir.is_dir() {
        bail!("{} not found — run `cr init` first", coderoom_dir.display(),);
    }

    let raw = read_project_raw(&coderoom_dir)?;
    let mut to_add = Vec::new();
    for addition in additions {
        validate_name(&addition.name)?;
        if raw.roles.contains_key(&addition.name) {
            continue;
        }
        to_add.push(addition.clone());
    }
    if to_add.is_empty() {
        return Ok(0);
    }

    let updated_config = append_roles_config_body(&coderoom_dir, &to_add)?;

    let roles_dir = coderoom_dir.join(ROLES_DIR);
    std::fs::create_dir_all(&roles_dir)
        .with_context(|| format!("creating {}", roles_dir.display()))?;
    for addition in &to_add {
        let priors_path = roles_dir.join(format!("{}.md", addition.name));
        if !priors_path.exists() {
            std::fs::write(&priors_path, render_role_template(&addition.name))
                .with_context(|| format!("writing {}", priors_path.display()))?;
        }
    }

    write_project_text(&coderoom_dir, &updated_config)?;

    Ok(to_add.len())
}

/// Print the configured roles, one per line, with engine + host marker.
///
/// Reads the merged `Config` (so the displayed engine/model reflects
/// the layered defaults), but never writes through it. Writes go via
/// [`read_project_raw`] / [`write_project_raw`] so user-level fields
/// don't accidentally end up in the committed project file.
pub fn list(project_root: &Path) -> Result<()> {
    let cfg = Config::load(project_root)?;

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
    let mut raw = read_project_raw(&coderoom_dir)?;

    if !raw.roles.contains_key(name) {
        bail!("no such role: @{name}");
    }
    if raw.host_role == name {
        bail!(
            "@{name} is the host role; change `host_role` in {CONFIG_FILE} first, \
             or use `cr role add` to introduce a replacement"
        );
    }

    raw.roles.remove(name);
    write_project_raw(&coderoom_dir, &raw)?;

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

/// Read just the project-layer raw shape — never the merged config.
/// This keeps role-edit round-trips free of user-layer values (e.g.
/// the user's `default_engine` won't accidentally end up in the
/// committed project file when `cr role add` writes back).
fn read_project_raw(coderoom_dir: &Path) -> Result<ProjectConfigRaw> {
    let path = coderoom_dir.join(CONFIG_FILE);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let raw: ProjectConfigRaw =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(raw)
}

fn write_project_raw(coderoom_dir: &Path, raw: &ProjectConfigRaw) -> Result<()> {
    let body = toml::to_string_pretty(raw).map_err(|e| anyhow!("serializing config.toml: {e}"))?;
    write_project_text(coderoom_dir, &body)?;
    Ok(())
}

fn write_project_text(coderoom_dir: &Path, body: &str) -> Result<()> {
    let path = coderoom_dir.join(CONFIG_FILE);
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn append_roles_config_body(coderoom_dir: &Path, additions: &[RoleAddition]) -> Result<String> {
    let path = coderoom_dir.join(CONFIG_FILE);
    let mut body =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    if !body.ends_with('\n') {
        body.push('\n');
    }
    if !body.ends_with("\n\n") {
        body.push('\n');
    }

    for addition in additions {
        writeln!(&mut body, "[roles.{}]", addition.name)
            .expect("writing role table header to string should not fail");
        if let Some(engine) = addition.engine {
            writeln!(&mut body, "engine = \"{}\"", engine.as_str())
                .expect("writing role engine to string should not fail");
        }
        if let Some(model) = &addition.model {
            let value = toml::Value::String(model.clone());
            writeln!(&mut body, "model = {value}")
                .expect("writing role model to string should not fail");
        }
        body.push('\n');
    }

    let _validated: ProjectConfigRaw =
        toml::from_str(&body).with_context(|| format!("validating {}", path.display()))?;
    Ok(body)
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
        let cfg = Config::load_test(tmp.path()).unwrap();
        assert!(cfg.roles.contains_key("backend"));
        assert!(coderoom.join(ROLES_DIR).join("backend.md").is_file());
    }

    #[test]
    fn add_persists_engine_and_model_overrides() {
        let tmp = fixture();
        add(tmp.path(), "security", Some(Engine::Codex), Some("o3")).unwrap();
        let cfg = Config::load_test(tmp.path()).unwrap();
        let entry = cfg.roles.get("security").unwrap();
        assert_eq!(entry.engine, Some(Engine::Codex));
        assert_eq!(entry.model.as_deref(), Some("o3"));
    }

    #[test]
    fn add_many_creates_roles_in_one_loadable_batch() {
        let tmp = fixture();
        let added = add_many(
            tmp.path(),
            &[
                RoleAddition {
                    name: "backend".into(),
                    engine: None,
                    model: None,
                },
                RoleAddition {
                    name: "security".into(),
                    engine: Some(Engine::Codex),
                    model: None,
                },
            ],
        )
        .unwrap();

        assert_eq!(added, 2);
        let cfg = Config::load_test(tmp.path()).unwrap();
        assert!(cfg.roles.contains_key("backend"));
        assert_eq!(cfg.roles["security"].engine, Some(Engine::Codex));
        assert!(tmp
            .path()
            .join(CODEROOM_DIR)
            .join(ROLES_DIR)
            .join("backend.md")
            .is_file());
        assert!(tmp
            .path()
            .join(CODEROOM_DIR)
            .join(ROLES_DIR)
            .join("security.md")
            .is_file());
    }

    #[test]
    fn add_many_skips_existing_roles() {
        let tmp = fixture();
        let added = add_many(
            tmp.path(),
            &[RoleAddition {
                name: "host".into(),
                engine: None,
                model: None,
            }],
        )
        .unwrap();

        assert_eq!(added, 0);
    }

    #[test]
    fn add_many_preserves_existing_config_text() {
        let tmp = fixture();
        let config_path = tmp.path().join(CODEROOM_DIR).join(CONFIG_FILE);
        fs::write(
            &config_path,
            r#"# keep this comment
default_engine = "cc"
budget_per_role_usd = 0.50
host_role = "host"

[roles.host]
"#,
        )
        .unwrap();

        add_many(
            tmp.path(),
            &[RoleAddition {
                name: "backend".into(),
                engine: None,
                model: None,
            }],
        )
        .unwrap();

        let body = fs::read_to_string(config_path).unwrap();
        assert!(body.contains("# keep this comment"));
        assert!(body.contains("[roles.backend]"));
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
        assert!(err.to_string().contains('@'));

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
        let cfg = Config::load_test(tmp.path()).unwrap();
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
