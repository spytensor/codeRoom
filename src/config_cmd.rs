//! Implementation of `cr config <show|edit|path>`.
//!
//! Three small operations on the layered config:
//!
//! - [`show`] prints the effective merged config plus the file paths
//!   that were read, so users can answer "which layer is supplying
//!   this value?" without `grep`.
//! - [`edit`] opens `$EDITOR` on a chosen layer file. Creates the
//!   parent directory and seeds a minimal commented stub for layers
//!   that don't exist yet (user, .local). The project layer is
//!   refused if `.coderoom/config.toml` is missing — `cr init` is the
//!   correct entry point there.
//! - [`path`] just prints the absolute path of the requested layer.
//!
//! Auto-gitignore: when `edit --local` creates `.coderoom/config.local.toml`,
//! it also ensures `.coderoom/.gitignore` contains a rule for the file.
//! The .gitignore is scoped to `.coderoom/` so we don't touch the
//! project's top-level ignore file.
//!
//! `cr config get / set` are intentionally not in this module — `edit`
//! covers the same cases without the design subtleties of writing
//! through arbitrary nested keys (array semantics, escape handling,
//! validation-on-write). They may land in v0.3 if there's demand.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::config::{Config, CODEROOM_DIR, CONFIG_FILE};
use crate::config_layered::{user_config_path, CONFIG_LOCAL_FILE};

/// Which config layer a command is targeting. Mirrors
/// [`crate::config_layered::Layer`] but excludes `Builtin`, since you
/// can't `edit` or print a `path` for the built-in defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerTarget {
    /// `~/.config/coderoom/config.toml` — personal preferences.
    User,
    /// `<project>/.coderoom/config.toml` — committed team contract.
    Project,
    /// `<project>/.coderoom/config.local.toml` — gitignored.
    Local,
}

/// Print the absolute path of the requested config layer file.
///
/// Does not check whether the file exists — `show` and `edit` are the
/// commands that care about existence.
pub fn path(layer: LayerTarget, project_root: &Path) -> Result<()> {
    let p = resolve_path(layer, project_root)?;
    println!("{}", p.display());
    Ok(())
}

/// Print the effective merged config and a footer of which layer files
/// contributed. Reads exactly the same layers as `cr start`, so the
/// values shown are what would actually be used.
pub fn show(project_root: &Path) -> Result<()> {
    show_with_user(project_root, user_config_path().as_deref())
}

/// Hermetic form of [`show`] that takes an explicit user-config path
/// (or `None` to skip the user layer). Tests use this so they don't
/// pick up the developer's real `~/.config/coderoom/config.toml`.
pub fn show_with_user(project_root: &Path, user_path: Option<&Path>) -> Result<()> {
    let cfg = crate::config_layered::load(project_root, user_path)?;
    print_effective(&cfg);
    print_layer_footer(user_path, project_root);
    Ok(())
}

/// Print one effective merged config value.
pub fn get(project_root: &Path, key: &str) -> Result<()> {
    let cfg = crate::config_layered::load(project_root, user_config_path().as_deref())?;
    let value = match key {
        "default_engine" | "defaults.engine" => cfg.default_engine.as_str().to_owned(),
        "default_model" | "defaults.model" => cfg
            .default_model
            .clone()
            .unwrap_or_else(|| "(engine default)".to_owned()),
        "budget_per_role_usd" | "defaults.budget_per_role_usd" => {
            format!("{:.2}", cfg.budget_per_role_usd)
        }
        "host_role" => cfg.host_role,
        other => bail!("unsupported key `{other}`"),
    };
    println!("{value}");
    Ok(())
}

/// Set a scalar config value in one writable layer.
pub fn set(layer: LayerTarget, project_root: &Path, key: &str, value: &str) -> Result<()> {
    let key = normalize_set_key(layer, key);
    let target = resolve_path(layer, project_root)?;
    match layer {
        LayerTarget::Project if !target.is_file() => bail!(
            "{} not found — run `cr init` first to bootstrap the project",
            target.display()
        ),
        LayerTarget::Local => {
            let coderoom_dir = project_root.join(CODEROOM_DIR);
            if !coderoom_dir.is_dir() {
                bail!(
                    "{} not found — run `cr init` first to bootstrap the project",
                    coderoom_dir.display()
                );
            }
            ensure_local_gitignored(&coderoom_dir)?;
        }
        LayerTarget::User | LayerTarget::Project => {}
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir for {}", target.display()))?;
    }

    let existing = std::fs::read_to_string(&target).unwrap_or_default();
    let mut root = toml::Value::Table(
        toml::from_str::<toml::map::Map<String, toml::Value>>(&existing)
            .unwrap_or_else(|_| toml::map::Map::new()),
    );
    set_value(&mut root, &key, parse_scalar(value))?;
    let text = toml::to_string_pretty(&root).context("serializing updated config")?;
    std::fs::write(&target, text).with_context(|| format!("writing {}", target.display()))?;
    println!("set {key} in {}", target.display());
    Ok(())
}

/// Open `$EDITOR` on the chosen layer file. Creates the parent
/// directory and seeds a commented stub when the file doesn't exist
/// yet (user / local). For `--local`, also ensures `.coderoom/.gitignore`
/// covers the file.
///
/// Refuses `--project` if `.coderoom/config.toml` is missing — `cr init`
/// is the right command for bootstrapping a project.
pub fn edit(layer: LayerTarget, project_root: &Path) -> Result<()> {
    // Validate the layer's prerequisites first, then seed any missing
    // file. We pick the editor LAST so that a missing $EDITOR doesn't
    // mask a more useful "run cr init first" error.
    let target = resolve_path(layer, project_root)?;
    match layer {
        LayerTarget::Project => {
            if !target.is_file() {
                bail!(
                    "{} not found — run `cr init` first to bootstrap the project",
                    target.display()
                );
            }
        }
        LayerTarget::User => ensure_seeded(&target, USER_STUB)?,
        LayerTarget::Local => {
            let coderoom_dir = project_root.join(CODEROOM_DIR);
            if !coderoom_dir.is_dir() {
                bail!(
                    "{} not found — run `cr init` first to bootstrap the project",
                    coderoom_dir.display()
                );
            }
            ensure_seeded(&target, LOCAL_STUB)?;
            ensure_local_gitignored(&coderoom_dir)?;
        }
    }

    let editor = pick_editor()?;
    let status = Command::new(&editor)
        .arg(&target)
        .status()
        .with_context(|| {
            format!(
                "launching $EDITOR={} on {}",
                editor.display(),
                target.display()
            )
        })?;
    if !status.success() {
        bail!(
            "editor `{}` exited with status {}",
            editor.display(),
            status
        );
    }
    Ok(())
}

// ---- Helpers -----------------------------------------------------------

fn resolve_path(layer: LayerTarget, project_root: &Path) -> Result<PathBuf> {
    match layer {
        LayerTarget::User => user_config_path().ok_or_else(|| {
            anyhow!(
                "no usable config dir on this OS — set $XDG_CONFIG_HOME \
                 or $HOME and retry"
            )
        }),
        LayerTarget::Project => Ok(project_root.join(CODEROOM_DIR).join(CONFIG_FILE)),
        LayerTarget::Local => Ok(project_root.join(CODEROOM_DIR).join(CONFIG_LOCAL_FILE)),
    }
}

fn parse_scalar(value: &str) -> toml::Value {
    if let Ok(value) = value.parse::<bool>() {
        return toml::Value::Boolean(value);
    }
    if let Ok(value) = value.parse::<i64>() {
        return toml::Value::Integer(value);
    }
    if let Ok(value) = value.parse::<f64>() {
        return toml::Value::Float(value);
    }
    toml::Value::String(value.to_owned())
}

fn normalize_set_key(layer: LayerTarget, key: &str) -> String {
    if layer == LayerTarget::User {
        match key {
            "default_engine" => "defaults.engine".to_owned(),
            "budget_per_role_usd" => "defaults.budget_per_role_usd".to_owned(),
            _ => key.to_owned(),
        }
    } else {
        key.to_owned()
    }
}

fn set_value(root: &mut toml::Value, key: &str, value: toml::Value) -> Result<()> {
    let parts = key.split('.').collect::<Vec<_>>();
    if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
        bail!("invalid key `{key}`");
    }
    let mut cursor = root;
    for part in &parts[..parts.len() - 1] {
        let table = cursor
            .as_table_mut()
            .ok_or_else(|| anyhow!("`{key}` crosses a non-table value"))?;
        cursor = table
            .entry((*part).to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    }
    let table = cursor
        .as_table_mut()
        .ok_or_else(|| anyhow!("`{key}` crosses a non-table value"))?;
    table.insert(parts[parts.len() - 1].to_owned(), value);
    Ok(())
}

const USER_STUB: &str = "\
# CodeRoom user config. Personal preferences that travel with you
# across projects. See `cr config --help`.
#
# [defaults]
# engine = \"cc\"             # cc | codex | gemini
# budget_per_role_usd = 0.30  # personal floor; project's budget is
#                              # min'd with this.
#
# [engines.cc]
# bin = \"/opt/claude/bin/claude\"  # override path to the engine binary
# api_key_env = \"ANTHROPIC_API_KEY\"
#
# [init]
# always_include = [\"security\"]   # roles you always want suggested
#
# [updates]
# check_on_start = true       # one background check per 24h
";

const LOCAL_STUB: &str = "\
# Project-local CodeRoom overrides. Always gitignored.
# Use this for machine-specific paths and auth refs that you don't
# want to commit. See `cr config --help`.
#
# [engines.cc]
# bin = \"/opt/claude/bin/claude\"
# api_key_env = \"ANTHROPIC_API_KEY\"
";

fn ensure_seeded(target: &Path, stub: &str) -> Result<()> {
    if target.exists() {
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir for {}", target.display()))?;
    }
    std::fs::write(target, stub).with_context(|| format!("writing {}", target.display()))?;
    Ok(())
}

/// Make sure `.coderoom/.gitignore` exists and lists the local config
/// file. Idempotent — appends only if the rule isn't already present.
fn ensure_local_gitignored(coderoom_dir: &Path) -> Result<()> {
    let ignore_path = coderoom_dir.join(".gitignore");
    let existing = std::fs::read_to_string(&ignore_path).unwrap_or_default();
    if existing
        .lines()
        .any(|line| line.trim() == CONFIG_LOCAL_FILE)
    {
        return Ok(());
    }
    let mut new = existing;
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    if new.is_empty() {
        new.push_str("# CodeRoom — auto-managed by `cr config edit --local`.\n");
    }
    new.push_str(CONFIG_LOCAL_FILE);
    new.push('\n');
    std::fs::write(&ignore_path, new)
        .with_context(|| format!("writing {}", ignore_path.display()))?;
    Ok(())
}

fn pick_editor() -> Result<PathBuf> {
    pick_editor_from(|k| std::env::var(k).ok())
}

/// Inner form so tests can inject env without `std::env::set_var` (which
/// is racy under cargo's parallel test runner).
fn pick_editor_from(get: impl Fn(&str) -> Option<String>) -> Result<PathBuf> {
    for key in ["VISUAL", "EDITOR"] {
        if let Some(v) = get(key) {
            if !v.is_empty() {
                return Ok(PathBuf::from(v));
            }
        }
    }
    bail!("no editor configured — set $EDITOR (e.g. `export EDITOR=vim`) and retry")
}

// ---- Pretty-print ------------------------------------------------------

fn print_effective(cfg: &Config) {
    println!("Effective configuration");
    println!("─────────────────────────");
    println!("default_engine      = {}", cfg.default_engine.as_str());
    if let Some(model) = &cfg.default_model {
        println!("default_model       = {model}");
    } else {
        println!("default_model       = (engine default)");
    }
    println!("budget_per_role_usd = {:.2}", cfg.budget_per_role_usd);
    println!("host_role           = {}", cfg.host_role);
    println!();
    println!("roles:");
    let mut names: Vec<&str> = cfg.roles.keys().map(String::as_str).collect();
    names.sort_unstable();
    if names.is_empty() {
        println!("  (none — use `cr role add`)");
    } else {
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
            let host = if cfg.is_host(name) { " (host)" } else { "" };
            println!("  @{name:<14} {engine:<6} / {model}{host}");
        }
    }
}

fn print_layer_footer(user_path: Option<&Path>, project_root: &Path) {
    println!();
    println!("Layers loaded:");
    let project_path = project_root.join(CODEROOM_DIR).join(CONFIG_FILE);
    let local_path = project_root.join(CODEROOM_DIR).join(CONFIG_LOCAL_FILE);

    print_layer_line("user   ", user_path);
    print_layer_line("project", Some(project_path.as_path()));
    print_layer_line("local  ", Some(local_path.as_path()));
}

fn print_layer_line(label: &str, p: Option<&Path>) {
    match p {
        Some(p) if p.exists() => println!("  {label}  {}", p.display()),
        Some(p) => println!("  {label}  {} (absent)", p.display()),
        None => println!("  {label}  (no path on this OS)"),
    }
}

// ---- Tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ROLES_DIR;
    use std::fs;
    use tempfile::TempDir;

    fn fixture() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
        fs::write(
            coderoom.join(CONFIG_FILE),
            "default_engine = \"cc\"\n\
             budget_per_role_usd = 0.50\n\
             host_role = \"host\"\n\n\
             [roles.host]\n",
        )
        .unwrap();
        fs::write(coderoom.join(ROLES_DIR).join("host.md"), "h").unwrap();
        tmp
    }

    #[test]
    fn resolve_paths_for_each_layer() {
        let tmp = fixture();
        let root = tmp.path();
        let project = resolve_path(LayerTarget::Project, root).unwrap();
        assert_eq!(project, root.join(CODEROOM_DIR).join(CONFIG_FILE));
        let local = resolve_path(LayerTarget::Local, root).unwrap();
        assert_eq!(local, root.join(CODEROOM_DIR).join(CONFIG_LOCAL_FILE));
    }

    #[test]
    fn ensure_seeded_creates_file_with_stub() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a/b/c.toml");
        ensure_seeded(&target, "stub-content\n").unwrap();
        assert!(target.is_file());
        assert_eq!(fs::read_to_string(&target).unwrap(), "stub-content\n");
    }

    #[test]
    fn ensure_seeded_is_no_op_when_file_exists() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("c.toml");
        fs::write(&target, "user-content").unwrap();
        ensure_seeded(&target, "stub").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "user-content");
    }

    #[test]
    fn ensure_local_gitignored_creates_when_missing() {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        fs::create_dir_all(&coderoom).unwrap();
        ensure_local_gitignored(&coderoom).unwrap();
        let body = fs::read_to_string(coderoom.join(".gitignore")).unwrap();
        assert!(body.contains(CONFIG_LOCAL_FILE));
    }

    #[test]
    fn ensure_local_gitignored_appends_to_existing() {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        fs::create_dir_all(&coderoom).unwrap();
        fs::write(coderoom.join(".gitignore"), "patches/\n").unwrap();
        ensure_local_gitignored(&coderoom).unwrap();
        let body = fs::read_to_string(coderoom.join(".gitignore")).unwrap();
        assert!(body.contains("patches/"));
        assert!(body.contains(CONFIG_LOCAL_FILE));
    }

    #[test]
    fn ensure_local_gitignored_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        fs::create_dir_all(&coderoom).unwrap();
        ensure_local_gitignored(&coderoom).unwrap();
        ensure_local_gitignored(&coderoom).unwrap();
        let body = fs::read_to_string(coderoom.join(".gitignore")).unwrap();
        // Only one occurrence of the rule, even after two calls.
        assert_eq!(
            body.matches(CONFIG_LOCAL_FILE).count(),
            1,
            "gitignore body: {body}"
        );
    }

    #[test]
    fn pick_editor_prefers_visual_over_editor() {
        let env = |k: &str| match k {
            "VISUAL" => Some("/usr/bin/vim".to_owned()),
            "EDITOR" => Some("/usr/bin/nano".to_owned()),
            _ => None,
        };
        assert_eq!(
            pick_editor_from(env).unwrap(),
            PathBuf::from("/usr/bin/vim")
        );
    }

    #[test]
    fn pick_editor_falls_back_to_editor_when_visual_empty() {
        let env = |k: &str| match k {
            "VISUAL" => Some(String::new()),
            "EDITOR" => Some("/usr/bin/nano".to_owned()),
            _ => None,
        };
        assert_eq!(
            pick_editor_from(env).unwrap(),
            PathBuf::from("/usr/bin/nano")
        );
    }

    #[test]
    fn pick_editor_errors_when_unset() {
        let env = |_: &str| None;
        let err = pick_editor_from(env).expect_err("no editor");
        assert!(err.to_string().contains("EDITOR"));
    }

    #[test]
    fn show_runs_against_a_minimal_project() {
        let tmp = fixture();
        // Smoke test — show writes to stdout; we just verify it
        // doesn't error against a valid project. Use `show_with_user`
        // with `None` so the developer's real config doesn't leak in.
        show_with_user(tmp.path(), None).unwrap();
    }

    #[test]
    fn path_subcommand_prints_each_layer() {
        let tmp = fixture();
        path(LayerTarget::Project, tmp.path()).unwrap();
        path(LayerTarget::Local, tmp.path()).unwrap();
        // user can be None in some CI envs; tolerate either.
        let _ = path(LayerTarget::User, tmp.path());
    }

    #[test]
    fn set_project_scalar_updates_config() {
        let tmp = fixture();
        set(
            LayerTarget::Project,
            tmp.path(),
            "budget_per_role_usd",
            "0.25",
        )
        .unwrap();
        let cfg = crate::config::Config::load_test(tmp.path()).unwrap();
        assert!((cfg.budget_per_role_usd - 0.25).abs() < 1e-9);
    }

    #[test]
    fn edit_project_refuses_when_config_missing() {
        let tmp = TempDir::new().unwrap();
        // No .coderoom/config.toml
        let err = edit(LayerTarget::Project, tmp.path()).expect_err("should refuse");
        assert!(err.to_string().contains("cr init"));
    }

    #[test]
    fn edit_local_refuses_when_coderoom_dir_missing() {
        let tmp = TempDir::new().unwrap();
        let err = edit(LayerTarget::Local, tmp.path()).expect_err("should refuse");
        assert!(err.to_string().contains("cr init"));
    }
}
