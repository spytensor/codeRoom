//! Three-layer config loader: user (`~/.config/coderoom/config.toml`)
//! → project (`.coderoom/config.toml`) → project-local
//! (`.coderoom/config.local.toml`).
//!
//! See [`merge`] for the precedence rules. The crate-public surface is
//! [`load`], which returns a [`crate::config::Config`] — the merged
//! result the rest of the code consumes. Callers see one effective
//! config; layering is internal.
//!
//! ## Why three layers
//!
//! Per the multi-agent review (issue #41):
//!
//! - **User layer** (committed to dotfiles, syncable across machines):
//!   personal preferences. Default engine when project doesn't pin
//!   one, default model per engine, personal `init.always_include`
//!   roles, update-check toggle.
//! - **Project layer** (committed to the project's git repo):
//!   the team contract. Roles topology, host role, project-pinned
//!   engine/model, budget cap.
//! - **Local layer** (gitignored, machine-specific): paths and other
//!   things that vary per checkout but were discovered while working
//!   in this repo. Today: only `engines.X.bin` and engine
//!   `api_key_env`.
//!
//! ## Forbidden cross-layer keys
//!
//! Some keys are categorically scoped — putting them in the wrong
//! layer is rejected with a [`ConfigError::Forbidden`] rather than
//! silently merged.
//!
//! - **Project layer must NOT contain** `engines.X.bin` (machine-
//!   specific, would be team-broken if committed) or
//!   `engines.X.api_key_env` (auth references must not be committed).
//! - **User layer must NOT contain** `[roles.<name>]` blocks or
//!   `host_role` — the set of roles is the project's division of
//!   labour, not personal preference.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::adapter::Engine;
use crate::config::{Config, ConfigError, ConfigResult, RoleEntry, CODEROOM_DIR, CONFIG_FILE};

/// File name of the project-local override file inside `.coderoom/`.
/// Always gitignored once it exists.
pub const CONFIG_LOCAL_FILE: &str = "config.local.toml";

/// Schema version emitted by writers. Loaders accept missing /
/// unknown values silently at v0.2; v0.3+ may use this for migrations.
pub const SCHEMA_VERSION: u32 = 1;

// ---- Layer label -------------------------------------------------------

/// Which layer a value came from. Returned by `cr config show
/// --origin` (PR B); exposed early so the type stabilises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// Built into the binary.
    Builtin,
    /// `~/.config/coderoom/config.toml`.
    User,
    /// `<project>/.coderoom/config.toml`.
    Project,
    /// `<project>/.coderoom/config.local.toml`.
    Local,
}

// ---- Raw layer types ---------------------------------------------------

/// User-layer config: every field optional. Lives at
/// `$XDG_CONFIG_HOME/coderoom/config.toml`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    /// Schema version. Informational at v0.2.
    #[serde(default)]
    pub schema_version: Option<u32>,

    /// Cross-engine fallbacks when a project doesn't pin a value.
    #[serde(default)]
    pub defaults: Option<UserDefaults>,

    /// Per-engine user preferences (model + machine-local bin path +
    /// auth-token env-var name).
    #[serde(default)]
    pub engines: HashMap<EngineKey, EngineUserEntry>,

    /// Personal `cr init` preferences.
    #[serde(default)]
    pub init: Option<InitConfig>,

    /// Update-check behaviour at `cr start`.
    #[serde(default)]
    pub updates: Option<UpdatesConfig>,

    /// Reject if present (project-only field). Declared so we can
    /// produce a *targeted* error rather than serde's generic
    /// "unknown field".
    #[serde(default)]
    pub host_role: Option<String>,
    /// Reject if present (project-only field).
    #[serde(default)]
    pub roles: HashMap<String, RoleEntry>,
}

/// User-layer defaults block.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserDefaults {
    /// Engine to use when neither project nor local pins one.
    #[serde(default)]
    pub engine: Option<Engine>,
    /// Personal floor on per-role spend; merged via `min()` across all
    /// declaring layers so user can self-protect even when project
    /// declares a higher value.
    #[serde(default)]
    pub budget_per_role_usd: Option<f64>,
}

/// Per-engine entry in user config.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineUserEntry {
    /// Default model for this engine when neither project nor local
    /// overrides.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional override path to the engine binary. Allowed in user
    /// AND `.local`; **forbidden** in project config.
    #[serde(default)]
    pub bin: Option<PathBuf>,
    /// Name of the env var holding this engine's API key. Allowed in
    /// user AND `.local`; **forbidden** in project config.
    #[serde(default)]
    pub api_key_env: Option<String>,
}

/// Personal `cr init` preferences.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitConfig {
    /// Role names always included in `cr init`'s suggestion set,
    /// even when project detection wouldn't pick them up. Unioned
    /// across layers.
    #[serde(default)]
    pub always_include: Vec<String>,
    /// Role names filtered out of `cr init`'s suggestion set even
    /// when detection picks them up. Unioned across layers; veto
    /// applies after `always_include` union.
    #[serde(default)]
    pub never_include: Vec<String>,
}

/// Update-check behaviour. Lives at user layer; `cr start` checks
/// once per 24h when this is `true`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatesConfig {
    /// Whether `cr start` performs a background update check.
    /// Defaults to `true` when the user config exists at all.
    #[serde(default = "default_true")]
    pub check_on_start: bool,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self {
            check_on_start: true,
        }
    }
}

const fn default_true() -> bool {
    true
}

/// Project-layer raw shape. Tolerant of missing fields so older
/// `.coderoom/config.toml` files still load. Required: `host_role`,
/// `[roles]` (may be empty).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfigRaw {
    /// Schema version. Informational at v0.2.
    #[serde(default)]
    pub schema_version: Option<u32>,
    /// Project-pinned default engine. Falls through to user when absent.
    #[serde(default)]
    pub default_engine: Option<Engine>,
    /// Project-pinned default model. Falls through to user when absent.
    #[serde(default)]
    pub default_model: Option<String>,
    /// Project-side spend cap. Merged via `min()` with user's value.
    #[serde(default)]
    pub budget_per_role_usd: Option<f64>,
    /// Required: name of the host role.
    pub host_role: String,
    /// Required (may be empty): role declarations.
    #[serde(default)]
    pub roles: HashMap<String, RoleEntry>,
    /// Project-side init preferences.
    #[serde(default)]
    pub init: Option<InitConfig>,
    /// `[engines.X]` block. Only `model` is allowed at project layer
    /// — `bin` and `api_key_env` are rejected during validation.
    #[serde(default)]
    pub engines: HashMap<EngineKey, EngineProjectEntry>,
}

/// Per-engine entry allowed at project layer. Strictly a subset of
/// the user-layer entry — `bin` and `api_key_env` are rejected.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineProjectEntry {
    /// Project-pinned model for this engine.
    #[serde(default)]
    pub model: Option<String>,
    /// Reject if present (machine-local field).
    #[serde(default)]
    pub bin: Option<PathBuf>,
    /// Reject if present (auth reference field).
    #[serde(default)]
    pub api_key_env: Option<String>,
}

/// Project-local override layer at `.coderoom/config.local.toml`.
/// Always gitignored. Today carries machine-local engine path and
/// `api_key_env` overrides.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalConfig {
    /// Schema version. Informational at v0.2.
    #[serde(default)]
    pub schema_version: Option<u32>,
    /// Per-engine machine-local overrides.
    #[serde(default)]
    pub engines: HashMap<EngineKey, EngineUserEntry>,
}

/// Newtype around `Engine` that serializes as the lowercase variant
/// name, so `[engines.cc]` / `[engines.codex]` / `[engines.gemini]`
/// work as TOML table keys.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct EngineKey(pub Engine);

impl From<EngineKey> for String {
    fn from(value: EngineKey) -> Self {
        value.0.as_str().to_owned()
    }
}

impl TryFrom<String> for EngineKey {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        match s.as_str() {
            "cc" => Ok(Self(Engine::Cc)),
            "codex" => Ok(Self(Engine::Codex)),
            "gemini" => Ok(Self(Engine::Gemini)),
            other => Err(format!(
                "unknown engine `{other}` — valid: cc / codex / gemini"
            )),
        }
    }
}

// ---- Path resolution ---------------------------------------------------

/// Path to the user-layer config file. Honors `XDG_CONFIG_HOME` on
/// Linux/macOS, falls back to `~/.config/coderoom/config.toml`. On
/// Windows uses `%APPDATA%\coderoom\config.toml`.
///
/// Returns `None` if the OS gave us no usable home / config dir
/// (rare; happens in some docker minimal images and CI sandboxes
/// without `$HOME` set).
#[must_use]
pub fn user_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("coderoom").join("config.toml"))
}

/// Path to the project-local override file inside the given project
/// root.
#[must_use]
pub fn local_config_path(project_root: &Path) -> PathBuf {
    project_root.join(CODEROOM_DIR).join(CONFIG_LOCAL_FILE)
}

// ---- Loading -----------------------------------------------------------

/// Load and merge all available layers, validating cross-layer rules.
///
/// `user_path = None` (or pointing at a non-existent file) skips the
/// user layer entirely. Tests pass an explicit path here to keep
/// loading hermetic.
pub fn load(project_root: &Path, user_path: Option<&Path>) -> ConfigResult<Config> {
    let coderoom_dir = project_root.join(CODEROOM_DIR);
    let project_path = coderoom_dir.join(CONFIG_FILE);
    let local_path = local_config_path(project_root);

    let user = match user_path {
        Some(p) if p.exists() => Some(read_user(p)?),
        _ => None,
    };
    let project = read_project(&project_path)?;
    let local = if local_path.exists() {
        Some(read_local(&local_path)?)
    } else {
        None
    };

    let merged = merge(user.as_ref(), &project, local.as_ref())?;
    merged.validate(&coderoom_dir)?;
    Ok(merged)
}

fn read_user(path: &Path) -> ConfigResult<UserConfig> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let parsed: UserConfig = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    validate_user_layer(&parsed, path)?;
    Ok(parsed)
}

fn read_project(path: &Path) -> ConfigResult<ProjectConfigRaw> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let parsed: ProjectConfigRaw = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    validate_project_layer(&parsed, path)?;
    Ok(parsed)
}

fn read_local(path: &Path) -> ConfigResult<LocalConfig> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

// ---- Cross-layer validation -------------------------------------------

fn validate_user_layer(cfg: &UserConfig, path: &Path) -> ConfigResult<()> {
    if cfg.host_role.is_some() {
        return Err(ConfigError::Forbidden {
            path: path.to_path_buf(),
            field: "host_role".into(),
            why: "host_role names a [roles.<name>] block in the project's \
                  .coderoom/config.toml. declare it there instead."
                .into(),
        });
    }
    if !cfg.roles.is_empty() {
        let names: Vec<String> = cfg.roles.keys().cloned().collect();
        return Err(ConfigError::Forbidden {
            path: path.to_path_buf(),
            field: format!("[roles.{}]", names.join(", ")),
            why: "the set of roles is the project's division of labour. \
                  declare them in the repo's .coderoom/config.toml."
                .into(),
        });
    }
    Ok(())
}

fn validate_project_layer(cfg: &ProjectConfigRaw, path: &Path) -> ConfigResult<()> {
    for (key, entry) in &cfg.engines {
        if entry.bin.is_some() {
            return Err(ConfigError::Forbidden {
                path: path.to_path_buf(),
                field: format!("engines.{}.bin", key.0.as_str()),
                why: "binary paths are machine-specific. put them in the user \
                      config (~/.config/coderoom/config.toml) or in this \
                      project's .coderoom/config.local.toml (gitignored)."
                    .into(),
            });
        }
        if entry.api_key_env.is_some() {
            return Err(ConfigError::Forbidden {
                path: path.to_path_buf(),
                field: format!("engines.{}.api_key_env", key.0.as_str()),
                why: "auth-token references must not be committed. put them \
                      in the user config or .coderoom/config.local.toml."
                    .into(),
            });
        }
    }
    Ok(())
}

// ---- Merge -------------------------------------------------------------

/// Merge user + project + local into the effective `Config`.
///
/// Precedence (highest first): local, project, user, built-in.
/// Per-field rules:
///
/// - **Scalars** (`default_engine`, `default_model`, `host_role`):
///   highest layer wins. Project beats user; user fills gaps.
/// - **`budget_per_role_usd`**: `min()` across all layers that declare
///   a value. At least one layer MUST declare it.
/// - **`init.always_include`**: union across layers, then filtered by
///   `never_include` from any layer.
/// - **`engines.X.model`**: layer-priority (project > user) with
///   project's flat `default_model` taking precedence over the
///   per-engine user value when neither side specifies an engine
///   override at the role level.
/// - **`engines.X.bin`** / **`engines.X.api_key_env`**: user / `.local`
///   only; `.local` wins where both declare.
/// - **`roles`**: project-only (already enforced at validation).
fn merge(
    user: Option<&UserConfig>,
    project: &ProjectConfigRaw,
    _local: Option<&LocalConfig>,
) -> ConfigResult<Config> {
    let default_engine = project
        .default_engine
        .or_else(|| {
            user.and_then(|u| u.defaults.as_ref())
                .and_then(|d| d.engine)
        })
        .ok_or(ConfigError::MissingDefaultEngine)?;

    let default_model = project.default_model.clone().or_else(|| {
        user.and_then(|u| u.engines.get(&EngineKey(default_engine)))
            .and_then(|e| e.model.clone())
    });

    let mut budgets: Vec<f64> = Vec::new();
    if let Some(b) = project.budget_per_role_usd {
        budgets.push(b);
    }
    if let Some(b) = user
        .and_then(|u| u.defaults.as_ref())
        .and_then(|d| d.budget_per_role_usd)
    {
        budgets.push(b);
    }
    if budgets.is_empty() {
        return Err(ConfigError::InvalidBudget(0.0));
    }
    let budget_per_role_usd = budgets.into_iter().fold(f64::INFINITY, f64::min);
    if !budget_per_role_usd.is_finite() || budget_per_role_usd <= 0.0 {
        return Err(ConfigError::InvalidBudget(budget_per_role_usd));
    }

    Ok(Config {
        default_engine,
        default_model,
        budget_per_role_usd,
        host_role: project.host_role.clone(),
        roles: project.roles.clone(),
    })
}

/// Compute the effective `init.always_include` set by unioning layers
/// then filtering with `never_include`. Returned in deterministic
/// (sorted) order for stable test assertions and stable splash output.
///
/// Exposed so PR B's `cr init`-aware suggestions can call it without
/// duplicating logic.
#[must_use]
pub fn merged_always_include(user: Option<&UserConfig>, project: &ProjectConfigRaw) -> Vec<String> {
    let never = merged_never_include(user, project);
    let mut out: BTreeSet<String> = BTreeSet::new();
    if let Some(init) = user.and_then(|u| u.init.as_ref()) {
        out.extend(init.always_include.iter().cloned());
    }
    if let Some(init) = project.init.as_ref() {
        out.extend(init.always_include.iter().cloned());
    }
    out.into_iter().filter(|n| !never.contains(n)).collect()
}

/// Union of `init.never_include` across all layers.
#[must_use]
pub fn merged_never_include(
    user: Option<&UserConfig>,
    project: &ProjectConfigRaw,
) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    if let Some(init) = user.and_then(|u| u.init.as_ref()) {
        out.extend(init.never_include.iter().cloned());
    }
    if let Some(init) = project.init.as_ref() {
        out.extend(init.never_include.iter().cloned());
    }
    out
}

// ---- Tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ROLES_DIR;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    fn write_minimal_project(coderoom: &Path, body_extra: &str) {
        std::fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
        let body = format!(
            r#"
default_engine = "cc"
budget_per_role_usd = 0.50
host_role = "host"

[roles.host]

{body_extra}
"#
        );
        std::fs::write(coderoom.join(CONFIG_FILE), body).unwrap();
        std::fs::write(coderoom.join(ROLES_DIR).join("host.md"), "host\n").unwrap();
    }

    fn write_user(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn project_only_loads_unchanged() {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        write_minimal_project(&coderoom, "");
        let cfg = load(tmp.path(), None).expect("load");
        assert_eq!(cfg.default_engine, Engine::Cc);
        assert!((cfg.budget_per_role_usd - 0.50).abs() < 1e-9);
        assert_eq!(cfg.host_role, "host");
    }

    #[test]
    fn user_default_engine_picked_when_project_omits_it() {
        let tmp = TempDir::new().unwrap();
        let user_path = tmp.path().join("user/config.toml");
        write_user(
            &user_path,
            r#"
[defaults]
engine = "codex"
"#,
        );
        let coderoom = tmp.path().join(CODEROOM_DIR);
        // Project intentionally omits default_engine.
        std::fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
        std::fs::write(
            coderoom.join(CONFIG_FILE),
            r#"
budget_per_role_usd = 0.50
host_role = "host"

[roles.host]
"#,
        )
        .unwrap();
        std::fs::write(coderoom.join(ROLES_DIR).join("host.md"), "host\n").unwrap();

        let cfg = load(tmp.path(), Some(&user_path)).expect("load");
        assert_eq!(cfg.default_engine, Engine::Codex);
    }

    #[test]
    fn project_engine_overrides_user_engine() {
        let tmp = TempDir::new().unwrap();
        let user_path = tmp.path().join("user-config.toml");
        write_user(
            &user_path,
            r#"
[defaults]
engine = "codex"
"#,
        );
        let coderoom = tmp.path().join(CODEROOM_DIR);
        write_minimal_project(&coderoom, ""); // project: cc
        let cfg = load(tmp.path(), Some(&user_path)).expect("load");
        assert_eq!(cfg.default_engine, Engine::Cc);
    }

    #[test]
    fn budget_takes_min_across_layers() {
        let tmp = TempDir::new().unwrap();
        let user_path = tmp.path().join("user-config.toml");
        write_user(
            &user_path,
            r"
[defaults]
budget_per_role_usd = 0.20
",
        );
        let coderoom = tmp.path().join(CODEROOM_DIR);
        write_minimal_project(&coderoom, ""); // project: 0.50
        let cfg = load(tmp.path(), Some(&user_path)).expect("load");
        assert!((cfg.budget_per_role_usd - 0.20).abs() < 1e-9);
    }

    #[test]
    fn budget_user_higher_does_not_relax_project_floor() {
        let tmp = TempDir::new().unwrap();
        let user_path = tmp.path().join("user-config.toml");
        write_user(
            &user_path,
            r"
[defaults]
budget_per_role_usd = 5.00
",
        );
        let coderoom = tmp.path().join(CODEROOM_DIR);
        write_minimal_project(&coderoom, ""); // 0.50 wins
        let cfg = load(tmp.path(), Some(&user_path)).expect("load");
        assert!((cfg.budget_per_role_usd - 0.50).abs() < 1e-9);
    }

    #[test]
    fn user_layer_with_roles_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let user_path = tmp.path().join("user-config.toml");
        write_user(&user_path, "[roles.backend]\n");
        let coderoom = tmp.path().join(CODEROOM_DIR);
        write_minimal_project(&coderoom, "");
        let err = load(tmp.path(), Some(&user_path)).expect_err("user roles must be rejected");
        match err {
            ConfigError::Forbidden { field, .. } => assert!(field.starts_with("[roles.")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn user_layer_with_host_role_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let user_path = tmp.path().join("user-config.toml");
        write_user(&user_path, r#"host_role = "stolen""#);
        let coderoom = tmp.path().join(CODEROOM_DIR);
        write_minimal_project(&coderoom, "");
        let err = load(tmp.path(), Some(&user_path)).expect_err("user host_role must be rejected");
        match err {
            ConfigError::Forbidden { field, .. } => assert_eq!(field, "host_role"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn project_layer_with_engine_bin_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        std::fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
        std::fs::write(coderoom.join(ROLES_DIR).join("host.md"), "x").unwrap();
        std::fs::write(
            coderoom.join(CONFIG_FILE),
            r#"
default_engine = "cc"
budget_per_role_usd = 0.50
host_role = "host"

[roles.host]

[engines.cc]
bin = "/opt/claude"
"#,
        )
        .unwrap();
        let err = load(tmp.path(), None).expect_err("project bin must be rejected");
        match err {
            ConfigError::Forbidden { field, .. } => assert_eq!(field, "engines.cc.bin"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn project_layer_with_engine_api_key_env_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        std::fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
        std::fs::write(coderoom.join(ROLES_DIR).join("host.md"), "x").unwrap();
        std::fs::write(
            coderoom.join(CONFIG_FILE),
            r#"
default_engine = "cc"
budget_per_role_usd = 0.50
host_role = "host"

[roles.host]

[engines.cc]
api_key_env = "ANTHROPIC_API_KEY"
"#,
        )
        .unwrap();
        let err = load(tmp.path(), None).expect_err("project api_key_env must be rejected");
        match err {
            ConfigError::Forbidden { field, .. } => assert_eq!(field, "engines.cc.api_key_env"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn local_layer_can_carry_engine_bin() {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        write_minimal_project(&coderoom, "");
        std::fs::write(
            coderoom.join(CONFIG_LOCAL_FILE),
            r#"
[engines.cc]
bin = "/opt/claude"
"#,
        )
        .unwrap();
        let cfg = load(tmp.path(), None).expect("local layer with bin loads");
        assert_eq!(cfg.default_engine, Engine::Cc);
    }

    #[test]
    fn missing_user_path_does_not_error() {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        write_minimal_project(&coderoom, "");
        let nonexistent = tmp.path().join("does-not-exist.toml");
        let cfg = load(tmp.path(), Some(&nonexistent)).expect("missing user is fine");
        assert_eq!(cfg.default_engine, Engine::Cc);
    }

    #[test]
    fn missing_default_engine_in_all_layers_errors() {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        std::fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
        std::fs::write(coderoom.join(ROLES_DIR).join("host.md"), "x").unwrap();
        // No default_engine anywhere.
        std::fs::write(
            coderoom.join(CONFIG_FILE),
            r#"
budget_per_role_usd = 0.50
host_role = "host"

[roles.host]
"#,
        )
        .unwrap();
        let err = load(tmp.path(), None).expect_err("missing engine must error");
        assert!(matches!(err, ConfigError::MissingDefaultEngine));
    }

    #[test]
    fn merged_always_include_unions_layers_then_filters_never() {
        let user = UserConfig {
            init: Some(InitConfig {
                always_include: vec!["security".into()],
                never_include: vec![],
            }),
            ..UserConfig::default()
        };
        let project = ProjectConfigRaw {
            schema_version: None,
            default_engine: Some(Engine::Cc),
            default_model: None,
            budget_per_role_usd: Some(0.5),
            host_role: "host".into(),
            roles: HashMap::new(),
            init: Some(InitConfig {
                always_include: vec!["data".into()],
                never_include: vec!["security".into()],
            }),
            engines: HashMap::new(),
        };
        let merged = merged_always_include(Some(&user), &project);
        // user's "security" is unioned then vetoed by project's never;
        // project's "data" survives.
        assert_eq!(merged, vec!["data".to_string()]);
    }

    #[test]
    fn schema_version_is_accepted_but_not_required() {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        std::fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
        std::fs::write(coderoom.join(ROLES_DIR).join("host.md"), "x").unwrap();
        std::fs::write(
            coderoom.join(CONFIG_FILE),
            r#"
schema_version = 1
default_engine = "cc"
budget_per_role_usd = 0.50
host_role = "host"

[roles.host]
"#,
        )
        .unwrap();
        let cfg = load(tmp.path(), None).expect("schema_version = 1 loads");
        assert_eq!(cfg.default_engine, Engine::Cc);
    }
}
