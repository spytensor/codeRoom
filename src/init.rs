//! `cr init` — bootstrap a project's `.coderoom/` directory.
//!
//! Default behaviour: scan the project (filename-only, **no network**),
//! print a transparent summary of what was found and what will be
//! written, ask `proceed? [Y/n]`, then generate the file tree.
//!
//! Non-interactive mode (`cr init -y`) skips the prompts and accepts
//! every default. `cr start` calls `run(.., InitOptions::auto())` when
//! `.coderoom/` is missing, so first-time users only need one command.
//!
//! Idempotent-by-refusing-to-overwrite: an existing `.coderoom/` errors
//! with a clear message rather than silently merging.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::{CODEROOM_DIR, CONFIG_FILE, ROLES_DIR};
use crate::detect::{self, StackSignal};

const DEFAULT_CONFIG_TOML: &str = include_str!("init_defaults/config.toml");
const DEFAULT_HOST_PRIORS: &str = include_str!("init_defaults/host.md");
const DEFAULT_SHARED_PRIORS: &str = include_str!("init_defaults/shared.md");
const DEFAULT_GITIGNORE: &str = include_str!("init_defaults/gitignore");
const DEFAULT_ROLE_TEMPLATE: &str = include_str!("init_defaults/role_template.md");

/// Knobs for [`run`].
#[derive(Debug, Clone, Copy)]
pub struct InitOptions {
    /// Skip every interactive prompt and accept defaults. Used by
    /// `cr init -y` and by `cr start`'s auto-init path.
    pub yes: bool,
    /// When `true`, the function prints a brief auto-init notice
    /// instead of the full transparent summary. Used by `cr start`.
    pub quiet_intro: bool,
}

impl InitOptions {
    /// Default for explicit `cr init` invocations: prompt before
    /// writing, full summary.
    #[must_use]
    pub const fn manual() -> Self {
        Self {
            yes: false,
            quiet_intro: false,
        }
    }

    /// Default for `cr start`'s auto-init path: no prompts, brief
    /// notice.
    #[must_use]
    pub const fn auto() -> Self {
        Self {
            yes: true,
            quiet_intro: true,
        }
    }
}

/// Initialize a project's `.coderoom/` directory at `project_root`.
pub fn run(project_root: &Path, options: InitOptions) -> Result<()> {
    let coderoom_dir = project_root.join(CODEROOM_DIR);
    if coderoom_dir.exists() {
        bail!(
            "{} already exists — refusing to overwrite. \
             edit it directly, or `rm -rf {}` to start over.",
            coderoom_dir.display(),
            coderoom_dir.display(),
        );
    }

    let scan = detect::scan(project_root);
    let installed = detect_installed_engines();
    let to_write = planned_files(&coderoom_dir, &scan.suggested_roles);

    if options.quiet_intro {
        print_auto_intro(&scan);
    } else {
        print_full_summary(project_root, &scan, &installed, &to_write);
    }

    if !options.yes && !confirm_proceed()? {
        println!("aborted. nothing was written.");
        return Ok(());
    }

    write_all(&coderoom_dir, &scan.suggested_roles)?;

    println!();
    println!("✓ wrote {}", coderoom_dir.display());
    println!();
    println!("next:  cr start");
    Ok(())
}

/// What `cr init` will create on disk, in render order.
fn planned_files(coderoom_dir: &Path, suggested_roles: &[&str]) -> Vec<PathBuf> {
    let mut paths = vec![
        coderoom_dir.join(CONFIG_FILE),
        coderoom_dir.join("shared.md"),
    ];
    let roles_dir = coderoom_dir.join(ROLES_DIR);
    for role in suggested_roles {
        paths.push(roles_dir.join(format!("{role}.md")));
    }
    paths.push(coderoom_dir.join(".gitignore"));
    paths
}

/// Brief notice used when `cr start` auto-inits — the user didn't ask
/// for a wall of text, just a heads-up that we're setting things up.
fn print_auto_intro(scan: &detect::ProjectScan) {
    let role_list = scan.suggested_roles.join(", @");
    println!("no .coderoom/ found — bootstrapping defaults: @{role_list} (engine: cc).");
    println!("  edit .coderoom/roles/<role>.md to give each one real project priors.");
    println!();
}

/// Full transparent summary for explicit `cr init`.
fn print_full_summary(
    project_root: &Path,
    scan: &detect::ProjectScan,
    installed: &InstalledEngines,
    to_write: &[PathBuf],
) {
    let project_name = project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("(this project)");

    println!();
    println!("scanning {project_name} … (local, no network)");
    println!();

    if scan.stack.is_empty() {
        println!("  no recognised stack signals at the project root.");
    } else {
        println!("  found:");
        for signal in &scan.stack {
            println!("    · {}", human_label(signal));
        }
    }
    println!();

    println!("  detected agent CLIs on PATH:");
    for (name, present, hint) in installed.summary() {
        if present {
            println!("    ✓ {name}");
        } else {
            println!("    ✗ {name}  →  {hint}");
        }
    }
    println!();

    println!("  i'll create:");
    println!("    {}/", project_root.join(CODEROOM_DIR).display());
    for path in to_write {
        if let Ok(rel) = path.strip_prefix(project_root.join(CODEROOM_DIR)) {
            println!("      {}", rel.display());
        }
    }
    println!();

    if let Some(line_count) = scan.existing_claude_md() {
        println!(
            "  found existing CLAUDE.md ({line_count} lines). \
             splitting it across role files is not yet automated;"
        );
        println!("  for now, leave it in place — coderoom doesn't touch it.");
        println!();
    }
}

/// `[Y/n]` prompt. Returns `true` for accept (default), `false` for
/// decline. Any other input re-prompts.
fn confirm_proceed() -> Result<bool> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    loop {
        print!("proceed? [Y/n]  ");
        stdout.flush().ok();
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let answer = line.trim().to_ascii_lowercase();
        match answer.as_str() {
            "" | "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("(answer y or n)"),
        }
    }
}

/// Materialize the `.coderoom/` skeleton on disk. Each role gets a
/// templated priors file with `{ROLE}` substituted.
fn write_all(coderoom_dir: &Path, roles: &[&str]) -> Result<()> {
    let roles_dir = coderoom_dir.join(ROLES_DIR);
    std::fs::create_dir_all(&roles_dir)
        .with_context(|| format!("creating {}", roles_dir.display()))?;

    write_file(&coderoom_dir.join(CONFIG_FILE), DEFAULT_CONFIG_TOML)?;
    write_file(&coderoom_dir.join("shared.md"), DEFAULT_SHARED_PRIORS)?;
    for role in roles {
        let path = roles_dir.join(format!("{role}.md"));
        let body = if *role == "host" {
            DEFAULT_HOST_PRIORS.to_string()
        } else {
            DEFAULT_ROLE_TEMPLATE.replace("{ROLE}", role)
        };
        write_file(&path, &body)?;
    }
    write_file(&coderoom_dir.join(".gitignore"), DEFAULT_GITIGNORE)?;

    // Append role entries beyond the default `host` so config.toml
    // declares everything we just scaffolded.
    let extra_roles: Vec<&&str> = roles.iter().filter(|r| **r != "host").collect();
    if !extra_roles.is_empty() {
        let mut tail = String::from("\n");
        for role in extra_roles {
            tail.push_str(&format!("[roles.{role}]\n"));
        }
        let config_path = coderoom_dir.join(CONFIG_FILE);
        let mut existing = std::fs::read_to_string(&config_path)
            .with_context(|| format!("reading {}", config_path.display()))?;
        if !existing.ends_with('\n') {
            existing.push('\n');
        }
        existing.push_str(&tail);
        std::fs::write(&config_path, existing)
            .with_context(|| format!("writing {}", config_path.display()))?;
    }
    Ok(())
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Human-readable bullet for a [`StackSignal`], used in the scan
/// summary. Kept here (not on the type) so the wording can evolve
/// independently of the detector's internals.
fn human_label(signal: &StackSignal) -> String {
    match signal {
        StackSignal::CargoToml => "Cargo.toml (Rust)".into(),
        StackSignal::GoMod => "go.mod (Go)".into(),
        StackSignal::PackageJson {
            has_ui_framework: true,
        } => "package.json (with UI framework)".into(),
        StackSignal::PackageJson {
            has_ui_framework: false,
        } => "package.json (no UI framework detected)".into(),
        StackSignal::PythonProject => "Python project (requirements.txt or pyproject.toml)".into(),
        StackSignal::JvmProject => "JVM project (pom.xml or build.gradle)".into(),
        StackSignal::Migrations => "migrations/ or db/ directory".into(),
        StackSignal::Prisma => "prisma/ directory".into(),
        StackSignal::GithubWorkflows => ".github/workflows/".into(),
        StackSignal::Dockerfile => "Dockerfile".into(),
        StackSignal::Terraform => "terraform/".into(),
        StackSignal::Pulumi => "pulumi/".into(),
        StackSignal::Kubernetes => "k8s/ or kubernetes/".into(),
        StackSignal::ExistingClaudeMd { line_count } => format!("CLAUDE.md ({line_count} lines)"),
    }
}

/// Result of probing `claude`/`codex`/`gemini` on `$PATH`.
struct InstalledEngines {
    cc: bool,
    codex: bool,
    gemini: bool,
}

impl InstalledEngines {
    fn summary(&self) -> [(&'static str, bool, &'static str); 3] {
        [
            (
                "claude (Anthropic)",
                self.cc,
                "https://docs.anthropic.com/claude-code",
            ),
            (
                "codex (OpenAI)",
                self.codex,
                "https://github.com/openai/codex",
            ),
            (
                "gemini (Google)",
                self.gemini,
                "https://github.com/google/gemini-cli",
            ),
        ]
    }
}

/// Probe `$PATH` for the three engine binaries. Each probe runs
/// `<bin> --version` once with all I/O captured, ~30 ms per call —
/// init runs once per project so this is cheap.
fn detect_installed_engines() -> InstalledEngines {
    InstalledEngines {
        cc: bin_present("claude"),
        codex: bin_present("codex"),
        gemini: bin_present("gemini"),
    }
}

fn bin_present(name: &str) -> bool {
    use std::process::Command;
    Command::new(name)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[test]
    fn init_yes_creates_minimal_valid_layout() {
        let tmp = TempDir::new().unwrap();
        run(tmp.path(), InitOptions::auto()).expect("auto init succeeds in fresh dir");

        let coderoom = tmp.path().join(CODEROOM_DIR);
        assert!(coderoom.is_dir());
        assert!(coderoom.join(CONFIG_FILE).is_file());
        assert!(coderoom.join("shared.md").is_file());
        assert!(coderoom.join(ROLES_DIR).join("host.md").is_file());
        assert!(coderoom.join(".gitignore").is_file());
    }

    #[test]
    fn init_yes_output_passes_config_validation() {
        let tmp = TempDir::new().unwrap();
        run(tmp.path(), InitOptions::auto()).expect("init");
        let cfg = Config::load(tmp.path()).expect("init output should be a valid config");
        assert_eq!(cfg.host_role, "host");
        assert!(cfg.is_host("host"));
    }

    #[test]
    fn init_refuses_to_overwrite_existing_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(CODEROOM_DIR)).unwrap();
        let err = run(tmp.path(), InitOptions::auto()).expect_err("should refuse to overwrite");
        assert!(err.to_string().contains("already exists"), "got: {err}");
    }

    #[test]
    fn detected_stack_creates_extra_roles_in_config() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        run(tmp.path(), InitOptions::auto()).expect("init");

        let cfg = Config::load(tmp.path()).expect("valid config");
        // Cargo.toml → host + backend + security
        assert!(cfg.roles.contains_key("host"));
        assert!(cfg.roles.contains_key("backend"));
        assert!(cfg.roles.contains_key("security"));

        let coderoom = tmp.path().join(CODEROOM_DIR);
        assert!(coderoom.join(ROLES_DIR).join("backend.md").is_file());
        assert!(coderoom.join(ROLES_DIR).join("security.md").is_file());
    }

    #[test]
    fn role_template_substitutes_role_name() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("go.mod"), "module x\n").unwrap();
        run(tmp.path(), InitOptions::auto()).expect("init");

        let backend_priors = std::fs::read_to_string(
            tmp.path()
                .join(CODEROOM_DIR)
                .join(ROLES_DIR)
                .join("backend.md"),
        )
        .unwrap();
        // Template's `{ROLE}` placeholder should be replaced.
        assert!(!backend_priors.contains("{ROLE}"));
        assert!(backend_priors.contains("@backend"));
    }

    #[test]
    fn planned_files_lists_in_render_order() {
        let coderoom = PathBuf::from("/tmp/p/.coderoom");
        let paths = planned_files(&coderoom, &["host", "backend"]);
        let display: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(
            display,
            vec![
                "/tmp/p/.coderoom/config.toml",
                "/tmp/p/.coderoom/shared.md",
                "/tmp/p/.coderoom/roles/host.md",
                "/tmp/p/.coderoom/roles/backend.md",
                "/tmp/p/.coderoom/.gitignore",
            ]
        );
    }
}
