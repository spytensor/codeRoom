//! Project-stack detection for `cr init`.
//!
//! Inspects only **filenames at the project root** plus a handful of
//! **top-level manifest keys** (e.g. `package.json` `dependencies`).
//! Never reads source-file contents, never traverses deeply, never
//! phones home. The user-facing message that goes with this scan is
//! `(local, no network)` — keep it true.
//!
//! See issue #32 for the rationale.

use std::path::{Path, PathBuf};

/// Roles considered "well-known" by the detector. Names match the
/// templates emitted by `cr role add`. Order matters: it controls the
/// order they appear in the splash and in the suggested-set output.
const HOST: &str = "host";
const BACKEND: &str = "backend";
const FRONTEND: &str = "frontend";
const DATA: &str = "data";
const SECURITY: &str = "security";
const DEVOPS: &str = "devops";
const CI: &str = "ci";

/// Distinct stack signals the detector recognises. Each variant maps
/// onto one or more "suggested roles" via [`ProjectScan::compute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackSignal {
    /// `Cargo.toml` at the project root.
    CargoToml,
    /// `go.mod` at the project root.
    GoMod,
    /// `package.json` at the project root.
    ///
    /// Distinguishing UI vs. service is the one place we read manifest
    /// *keys* (not values, not source).
    PackageJson {
        /// `true` when one of `react`, `vue`, `svelte`, `next`, `nuxt`,
        /// `@angular/core`, `solid-js`, `preact`, `lit`, or `qwik`
        /// appears as a key in `dependencies` or `devDependencies`.
        has_ui_framework: bool,
    },
    /// `requirements.txt` or `pyproject.toml`.
    PythonProject,
    /// `pom.xml` or `build.gradle{,.kts}`.
    JvmProject,
    /// `migrations/` or `db/` directory at the project root.
    Migrations,
    /// `prisma/` directory at the project root.
    Prisma,
    /// `.github/workflows/` directory exists.
    GithubWorkflows,
    /// `Dockerfile` (any case) at the project root.
    Dockerfile,
    /// `terraform/` directory at the project root.
    Terraform,
    /// `pulumi/` directory at the project root.
    Pulumi,
    /// `k8s/`, `kubernetes/`, or `.k8s/` directory at the project root.
    Kubernetes,
    /// Existing `CLAUDE.md` at the project root. The line count is
    /// reported so init can phrase its split offer as
    /// "found CLAUDE.md (1,238 lines)".
    ExistingClaudeMd {
        /// Newline count of the file, used by init's split-offer copy.
        line_count: usize,
    },
}

/// Detector output, consumed by `cr init` and the advanced wizard.
#[derive(Debug, Clone)]
pub struct ProjectScan {
    /// The directory that was scanned (canonical-or-absolute path).
    pub root: PathBuf,
    /// Signals found, in deterministic order (defined by `scan`).
    pub stack: Vec<StackSignal>,
    /// Roles the detector suggests, in stable display order
    /// (`host` first, then domain, then ops).
    pub suggested_roles: Vec<&'static str>,
}

impl ProjectScan {
    /// Whether the user already has a CLAUDE.md the wizard should
    /// offer to split.
    #[must_use]
    pub fn existing_claude_md(&self) -> Option<usize> {
        self.stack.iter().find_map(|s| match s {
            StackSignal::ExistingClaudeMd { line_count } => Some(*line_count),
            _ => None,
        })
    }

    /// Compute the suggested-roles set from the scan's signals.
    ///
    /// Pure function: same `stack` always yields the same vector,
    /// across runs and machines. No reliance on filesystem ordering.
    fn compute(stack: &[StackSignal]) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::with_capacity(6);
        out.push(HOST);

        let has_backend_lang = stack.iter().any(|s| {
            matches!(
                s,
                StackSignal::CargoToml
                    | StackSignal::GoMod
                    | StackSignal::PythonProject
                    | StackSignal::JvmProject
                    | StackSignal::PackageJson {
                        has_ui_framework: false
                    }
            )
        });
        let has_frontend = stack.iter().any(|s| {
            matches!(
                s,
                StackSignal::PackageJson {
                    has_ui_framework: true
                }
            )
        });
        let has_data = stack
            .iter()
            .any(|s| matches!(s, StackSignal::Migrations | StackSignal::Prisma));
        let has_devops = stack.iter().any(|s| {
            matches!(
                s,
                StackSignal::Dockerfile
                    | StackSignal::Terraform
                    | StackSignal::Pulumi
                    | StackSignal::Kubernetes
            )
        });
        let has_ci = stack
            .iter()
            .any(|s| matches!(s, StackSignal::GithubWorkflows));

        if has_backend_lang {
            out.push(BACKEND);
        }
        if has_frontend {
            out.push(FRONTEND);
        }
        if has_data {
            out.push(DATA);
        }
        // @security is suggested whenever there's server-side or data
        // code — it's the role most likely to catch real issues.
        if has_backend_lang || has_data {
            out.push(SECURITY);
        }
        if has_devops {
            out.push(DEVOPS);
        }
        if has_ci {
            out.push(CI);
        }
        out
    }
}

/// Detect stack signals at `root`.
///
/// **Filename-globs and top-level manifest keys only.** Never reads
/// source-file contents. Returns even for empty / unrecognised
/// projects (in which case `stack` is empty and `suggested_roles` is
/// just `["host"]`).
#[must_use]
pub fn scan(root: &Path) -> ProjectScan {
    let mut stack: Vec<StackSignal> = Vec::new();

    if root.join("Cargo.toml").is_file() {
        stack.push(StackSignal::CargoToml);
    }
    if root.join("go.mod").is_file() {
        stack.push(StackSignal::GoMod);
    }
    if root.join("package.json").is_file() {
        let has_ui = package_json_has_ui_framework(&root.join("package.json"));
        stack.push(StackSignal::PackageJson {
            has_ui_framework: has_ui,
        });
    }
    if root.join("requirements.txt").is_file() || root.join("pyproject.toml").is_file() {
        stack.push(StackSignal::PythonProject);
    }
    if root.join("pom.xml").is_file()
        || root.join("build.gradle").is_file()
        || root.join("build.gradle.kts").is_file()
    {
        stack.push(StackSignal::JvmProject);
    }

    if dir_exists(root, "migrations") || dir_exists(root, "db") {
        stack.push(StackSignal::Migrations);
    }
    if dir_exists(root, "prisma") {
        stack.push(StackSignal::Prisma);
    }
    if root.join(".github").join("workflows").is_dir() {
        stack.push(StackSignal::GithubWorkflows);
    }
    if root.join("Dockerfile").is_file() || root.join("dockerfile").is_file() {
        stack.push(StackSignal::Dockerfile);
    }
    if dir_exists(root, "terraform") {
        stack.push(StackSignal::Terraform);
    }
    if dir_exists(root, "pulumi") {
        stack.push(StackSignal::Pulumi);
    }
    if dir_exists(root, "k8s") || dir_exists(root, "kubernetes") || dir_exists(root, ".k8s") {
        stack.push(StackSignal::Kubernetes);
    }

    if let Some(line_count) = claude_md_line_count(&root.join("CLAUDE.md")) {
        stack.push(StackSignal::ExistingClaudeMd { line_count });
    }

    let suggested_roles = ProjectScan::compute(&stack);
    ProjectScan {
        root: root.to_path_buf(),
        stack,
        suggested_roles,
    }
}

fn dir_exists(root: &Path, name: &str) -> bool {
    root.join(name).is_dir()
}

/// Read just the `dependencies` and `devDependencies` keys of a
/// `package.json` and return whether any well-known UI framework is
/// listed. Falls back to `false` on any parse error.
fn package_json_has_ui_framework(path: &Path) -> bool {
    const UI_FRAMEWORKS: &[&str] = &[
        "react",
        "vue",
        "svelte",
        "next",
        "nuxt",
        "@angular/core",
        "solid-js",
        "preact",
        "lit",
        "qwik",
    ];
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    let mut keys: Vec<String> = Vec::new();
    for field in ["dependencies", "devDependencies"] {
        if let Some(map) = parsed.get(field).and_then(|v| v.as_object()) {
            keys.extend(map.keys().cloned());
        }
    }
    keys.iter().any(|k| UI_FRAMEWORKS.contains(&k.as_str()))
}

fn claude_md_line_count(path: &Path) -> Option<usize> {
    let content = std::fs::read_to_string(path).ok()?;
    Some(content.lines().count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::TempDir;

    fn empty_project() -> TempDir {
        TempDir::new().unwrap()
    }

    #[test]
    fn empty_project_only_suggests_host() {
        let tmp = empty_project();
        let scan = scan(tmp.path());
        assert!(scan.stack.is_empty());
        assert_eq!(scan.suggested_roles, vec!["host"]);
    }

    #[test]
    fn rust_project_suggests_backend_security() {
        let tmp = empty_project();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let scan = scan(tmp.path());
        assert_eq!(scan.stack, vec![StackSignal::CargoToml]);
        assert_eq!(scan.suggested_roles, vec!["host", "backend", "security"]);
    }

    #[test]
    fn package_json_with_react_suggests_frontend() {
        let tmp = empty_project();
        fs::write(
            tmp.path().join("package.json"),
            r#"{"dependencies":{"react":"^18"}}"#,
        )
        .unwrap();
        let scan = scan(tmp.path());
        assert!(scan.stack.iter().any(|s| matches!(
            s,
            StackSignal::PackageJson {
                has_ui_framework: true
            }
        )));
        assert!(scan.suggested_roles.contains(&"frontend"));
        // No backend lang signal here → no @backend, no @security
        assert!(!scan.suggested_roles.contains(&"backend"));
        assert!(!scan.suggested_roles.contains(&"security"));
    }

    #[test]
    fn package_json_without_ui_suggests_backend() {
        let tmp = empty_project();
        fs::write(
            tmp.path().join("package.json"),
            r#"{"dependencies":{"express":"^4"}}"#,
        )
        .unwrap();
        let scan = scan(tmp.path());
        assert!(scan.suggested_roles.contains(&"backend"));
        assert!(!scan.suggested_roles.contains(&"frontend"));
    }

    #[test]
    fn malformed_package_json_falls_back_to_no_ui() {
        let tmp = empty_project();
        fs::write(tmp.path().join("package.json"), "this is not json").unwrap();
        let scan = scan(tmp.path());
        // Still recognised as PackageJson, but UI flag is false
        assert!(scan.stack.iter().any(|s| matches!(
            s,
            StackSignal::PackageJson {
                has_ui_framework: false
            }
        )));
    }

    #[test]
    fn migrations_dir_suggests_data_and_security() {
        let tmp = empty_project();
        fs::create_dir_all(tmp.path().join("migrations")).unwrap();
        let scan = scan(tmp.path());
        assert!(scan.suggested_roles.contains(&"data"));
        assert!(scan.suggested_roles.contains(&"security"));
    }

    #[test]
    fn github_workflows_suggests_ci() {
        let tmp = empty_project();
        fs::create_dir_all(tmp.path().join(".github").join("workflows")).unwrap();
        let scan = scan(tmp.path());
        assert!(scan.suggested_roles.contains(&"ci"));
        // No backend/frontend/data signal → no @security inferred
        assert!(!scan.suggested_roles.contains(&"security"));
    }

    #[test]
    fn dockerfile_or_terraform_suggests_devops() {
        let tmp = empty_project();
        fs::write(tmp.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        let scan = scan(tmp.path());
        assert!(scan.suggested_roles.contains(&"devops"));
    }

    #[test]
    fn existing_claude_md_is_reported_with_line_count() {
        let tmp = empty_project();
        let body = "line1\nline2\nline3\n";
        fs::write(tmp.path().join("CLAUDE.md"), body).unwrap();
        let scan = scan(tmp.path());
        assert_eq!(scan.existing_claude_md(), Some(3));
    }

    #[test]
    fn full_stack_project_suggests_full_set_in_stable_order() {
        let tmp = empty_project();
        fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        fs::write(
            tmp.path().join("package.json"),
            r#"{"dependencies":{"react":"^18"}}"#,
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("migrations")).unwrap();
        fs::create_dir_all(tmp.path().join(".github").join("workflows")).unwrap();
        fs::write(tmp.path().join("Dockerfile"), "FROM scratch\n").unwrap();

        let scan = scan(tmp.path());
        assert_eq!(
            scan.suggested_roles,
            vec!["host", "backend", "frontend", "data", "security", "devops", "ci"]
        );
    }

    #[test]
    fn scan_is_deterministic() {
        let tmp = empty_project();
        fs::write(tmp.path().join("go.mod"), "module x\n").unwrap();
        fs::create_dir_all(tmp.path().join("k8s")).unwrap();
        let a = scan(tmp.path());
        let b = scan(tmp.path());
        assert_eq!(a.suggested_roles, b.suggested_roles);
        assert_eq!(a.stack, b.stack);
    }
}
