//! Project-level configuration loaded from `.coderoom/config.toml`.
//!
//! A project's `.coderoom/` layout (per `docs/architecture.md`):
//!
//! ```text
//! .coderoom/
//! ├── config.toml             # this file
//! ├── roles/<name>.md         # per-role base priors
//! ├── shared.md               # priors loaded by every role (optional)
//! ├── patches/<role>/...      # session-time corrections
//! └── journal/YYYY-MM-DD/...  # daily learnings, role-written
//! ```
//!
//! `config.toml` shape:
//!
//! ```toml
//! default_engine = "cc"          # cc | codex | gemini
//! default_model = "opus"         # optional; engine-specific id
//! permission_mode = "ask"        # ask | auto | bypass
//! budget_per_role_usd = 0.50     # hard cap fed to each engine
//! host_role = "pm"               # role that catches un-addressed text
//!
//! [roles.pm]
//! # engine inherits default_engine; model inherits default_model
//!
//! [roles.security]
//! engine = "codex"
//! model = "o3"
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::adapter::{Engine, PermissionMode, RoleConfig};

/// Standard subdirectory inside a project that holds CodeRoom state.
pub const CODEROOM_DIR: &str = ".coderoom";

/// File name of the project-level config inside [`CODEROOM_DIR`].
pub const CONFIG_FILE: &str = "config.toml";

/// Subdirectory holding per-role priors files.
pub const ROLES_DIR: &str = "roles";

/// Project-level config loaded from `.coderoom/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// Engine used for any role that doesn't override.
    pub default_engine: Engine,
    /// Model id used for any role that doesn't override. Engine-specific
    /// (e.g. `"opus"` for `cc`, `"o3"` for `codex`).
    #[serde(default)]
    pub default_model: Option<String>,
    /// Default permission mode for any role that doesn't override.
    #[serde(default = "default_permission_mode")]
    pub permission_mode: PermissionMode,
    /// Per-role budget cap in USD, fed to each engine via its native
    /// budget flag (`--max-budget-usd` for `cc`, etc.).
    pub budget_per_role_usd: f64,
    /// Name of the role that catches un-addressed text in the REPL.
    /// Must exist in [`Self::roles`].
    pub host_role: String,
    /// Per-role overrides. Each key is a role name; the table allows
    /// engine/model to differ from the defaults. Entries are optional —
    /// a role with no entry uses the defaults.
    #[serde(default)]
    pub roles: HashMap<String, RoleEntry>,
}

/// Per-role overrides in `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RoleEntry {
    /// Engine override. `None` ⇒ use [`Config::default_engine`].
    #[serde(default)]
    pub engine: Option<Engine>,
    /// Model override. `None` ⇒ use [`Config::default_model`].
    #[serde(default)]
    pub model: Option<String>,
    /// Permission mode override. `None` ⇒ use [`Config::permission_mode`].
    #[serde(default)]
    pub permission_mode: Option<PermissionMode>,
}

const fn default_permission_mode() -> PermissionMode {
    PermissionMode::Ask
}

/// Errors raised while loading or validating a config.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// `.coderoom/config.toml` is missing or unreadable.
    #[error("could not read {path}: {source}")]
    Read {
        /// Absolute path that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// `.coderoom/config.toml` content is not valid TOML / shape.
    #[error("could not parse {path}: {source}")]
    Parse {
        /// Absolute path that failed to parse.
        path: PathBuf,
        /// Underlying TOML deserialization error.
        #[source]
        source: toml::de::Error,
    },
    /// `host_role` was not declared as a role.
    #[error("host_role `{host}` is not declared in [roles] (declared: {declared:?})")]
    MissingHostRole {
        /// Value of `host_role` that didn't resolve.
        host: String,
        /// Role names that *were* declared.
        declared: Vec<String>,
    },
    /// A role exists in config but its priors file is missing on disk.
    #[error("role `{role}` is missing priors file at {expected}")]
    MissingPriors {
        /// Role name that lacks priors.
        role: String,
        /// Path the loader checked.
        expected: PathBuf,
    },
    /// `budget_per_role_usd` was not a positive finite number.
    #[error("budget_per_role_usd must be positive and finite, got {0}")]
    InvalidBudget(f64),
    /// A categorical scoping rule was violated: a key only allowed in
    /// one specific layer was found in a different layer (e.g.
    /// `engines.cc.bin` in committed project config, or `[roles]` in
    /// user config).
    #[error("`{field}` is not allowed in {path}: {why}")]
    Forbidden {
        /// Path of the offending file.
        path: PathBuf,
        /// Field path (e.g. `engines.cc.bin`).
        field: String,
        /// Human-readable explanation pointing the user at the right
        /// layer.
        why: String,
    },
    /// No layer declared `default_engine`. Built-in fallback isn't
    /// applied because choosing an engine without consent is a real
    /// failure mode.
    #[error(
        "no default_engine is declared. set it in either user config \
         (~/.config/coderoom/config.toml under [defaults] engine = \"cc\") \
         or project config (.coderoom/config.toml `default_engine = \"cc\"`)."
    )]
    MissingDefaultEngine,
}

/// Convenience alias for config results.
pub type ConfigResult<T> = Result<T, ConfigError>;

impl Config {
    /// Load and validate a project's `.coderoom/config.toml`.
    ///
    /// `project_root` is the directory containing `.coderoom/` — typically
    /// the user's project repo root. Validation includes:
    ///
    /// 1. TOML parses into the documented shape.
    /// 2. `budget_per_role_usd` is positive + finite.
    /// 3. `host_role` is one of the declared roles.
    /// 4. Every declared role has a priors file at
    ///    `.coderoom/roles/<role>.md`.
    pub fn load(project_root: impl AsRef<Path>) -> ConfigResult<Self> {
        // Delegate to the layered loader. Production code resolves
        // the user-config path from $XDG_CONFIG_HOME / ~/.config etc.;
        // tests can call `crate::config_layered::load` directly to
        // pass a hermetic user path (or `None`).
        crate::config_layered::load(
            project_root.as_ref(),
            crate::config_layered::user_config_path().as_deref(),
        )
    }

    /// Validate the in-memory config against on-disk state. Used by
    /// [`Self::load`]; exposed so tests can validate hand-built configs.
    pub fn validate(&self, coderoom_dir: &Path) -> ConfigResult<()> {
        if !self.budget_per_role_usd.is_finite() || self.budget_per_role_usd <= 0.0 {
            return Err(ConfigError::InvalidBudget(self.budget_per_role_usd));
        }

        if !self.roles.contains_key(&self.host_role) {
            let mut declared: Vec<String> = self.roles.keys().cloned().collect();
            declared.sort();
            return Err(ConfigError::MissingHostRole {
                host: self.host_role.clone(),
                declared,
            });
        }

        for name in self.roles.keys() {
            let priors = priors_path_for(coderoom_dir, name);
            if !priors.exists() {
                return Err(ConfigError::MissingPriors {
                    role: name.clone(),
                    expected: priors,
                });
            }
        }

        Ok(())
    }

    /// Whether the given role is the configured host.
    #[must_use]
    pub fn is_host(&self, role: &str) -> bool {
        self.host_role == role
    }

    /// Build an [`adapter::RoleConfig`](crate::adapter::RoleConfig) for
    /// the named role. Resolves engine/model from per-role overrides
    /// falling back to defaults.
    ///
    /// Returns `None` if the role is not declared in the config.
    #[must_use]
    pub fn role_config(&self, name: &str, coderoom_dir: &Path) -> Option<RoleConfig> {
        let entry = self.roles.get(name)?;
        let engine = entry.engine.unwrap_or(self.default_engine);
        let permission_mode = entry.permission_mode.unwrap_or(match engine {
            // Codex/Gemini approval responders are not wired yet. Existing
            // projects created before per-role permission modes may have
            // `engine = "codex"` with no explicit override; keep those roles
            // startable while explicit ask/auto settings still fail in the
            // adapters.
            Engine::Codex | Engine::Gemini => PermissionMode::Bypass,
            Engine::Cc => self.permission_mode,
        });
        Some(RoleConfig {
            name: name.to_owned(),
            engine,
            model: entry.model.clone().or_else(|| self.default_model.clone()),
            priors_path: priors_path_for(coderoom_dir, name),
            budget_usd: self.budget_per_role_usd,
            permission_mode,
            permission_policy_path: None,
            permission_socket_path: None,
        })
    }

    /// Iterator over declared role names, in arbitrary order.
    pub fn role_names(&self) -> impl Iterator<Item = &str> {
        self.roles.keys().map(String::as_str)
    }

    /// Test-only hermetic loader that skips the user layer entirely.
    /// Used by unit tests so they don't pick up the developer's
    /// real `~/.config/coderoom/config.toml` and become flaky on
    /// machines where the user has actually configured CodeRoom.
    #[cfg(test)]
    pub(crate) fn load_test(project_root: impl AsRef<Path>) -> ConfigResult<Self> {
        crate::config_layered::load(project_root.as_ref(), None)
    }
}

/// Path where the priors file for `role` lives, given the project's
/// `.coderoom/` directory.
fn priors_path_for(coderoom_dir: &Path, role: &str) -> PathBuf {
    coderoom_dir.join(ROLES_DIR).join(format!("{role}.md"))
}

#[cfg(test)]
mod tests;
