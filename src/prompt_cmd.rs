//! Implementation of `cr prompt show`.

use std::path::Path;

use anyhow::{bail, Result};

use crate::config::{Config, CODEROOM_DIR};
use crate::priors;

/// Render the effective prompt for `role` exactly as the REPL would compose it.
pub fn render(project_root: &Path, role: &str) -> Result<String> {
    let role = role.strip_prefix('@').unwrap_or(role);
    let cfg = Config::load(project_root)?;
    if !cfg.roles.contains_key(role) {
        bail!("role `{role}` is not declared in .coderoom/config.toml");
    }
    priors::compose_for(&project_root.join(CODEROOM_DIR), role)
}

/// Print the effective prompt for `role`.
pub fn show(project_root: &Path, role: &str) -> Result<()> {
    print!("{}", render(project_root, role)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::config::{CONFIG_FILE, ROLES_DIR};

    fn fixture() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
        fs::write(
            coderoom.join(CONFIG_FILE),
            r#"
default_engine = "cc"
permission_mode = "ask"
budget_per_role_usd = 0.50
host_role = "host"

[roles.host]
[roles.backend]
"#,
        )
        .unwrap();
        fs::write(coderoom.join(ROLES_DIR).join("host.md"), "HOST_PRIORS").unwrap();
        fs::write(
            coderoom.join(ROLES_DIR).join("backend.md"),
            "BACKEND_PRIORS",
        )
        .unwrap();
        tmp
    }

    #[test]
    fn render_accepts_at_prefixed_role() {
        let tmp = fixture();
        let prompt = render(tmp.path(), "@backend").unwrap();
        assert!(prompt.contains("# CodeRoom kernel protocol"));
        assert!(prompt.contains("BACKEND_PRIORS"));
        assert!(prompt.contains("Source: .coderoom/roles/backend.md"));
    }

    #[test]
    fn render_rejects_undeclared_role() {
        let tmp = fixture();
        let err = render(tmp.path(), "security").unwrap_err();
        assert!(
            format!("{err:#}").contains("role `security` is not declared"),
            "{err:#}"
        );
    }
}
