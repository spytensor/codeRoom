//! Project health checks for CodeRoom-managed files.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::config::CODEROOM_DIR;

/// Options for `cr doctor`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DoctorOptions {
    /// Rewrite files when a safe, exact fix is available.
    pub fix: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SharedProtocolStatus {
    Clean,
    LegacyExact,
    LegacyMixed,
    LegacyEdited,
}

const PROJECT_SHARED_TEMPLATE: &str = "\
# Team-wide priors

Add project standards, product context, and engineering preferences that every role should share.

Examples:

- Preferred frameworks, libraries, and architectural boundaries.
- Testing, migration, release, or review standards.
- Domain vocabulary and product constraints.
";

const LEGACY_SHARED_PROTOCOL: &str = "\
# Shared CodeRoom protocol

You are running inside CodeRoom, a local multi-role coordination shell. The user remains accountable for all project changes; you provide role-scoped analysis, trade-offs, patches, and verification steps.

Roles are addressed as `@name`. If a user writes `@backend ...`, only that role receives the message. In role replies, only a physical line that starts with `@name` (or a line-start list item like `- @a @b`) is a delegation that CodeRoom may route as `From @backend: <text>`. Use plain role names, not `@name`, for attribution, status, risk tables, or summaries.

Bare user text goes to the current host role. The host is a normal role, not a manager with special authority. Escalate to the host when you need direction, conflicting constraints resolved, or user confirmation.

Use `/patch` facts as explicit user-written corrections. They override older priors until the user edits or removes them. Use `/journal` entries as recent memory, but only rely on claims that cite a transcript anchor or repository path.

Your effective prompt is assembled from shared priors, your role priors, active patches, recent journal entries, and a team roster. Keep replies concise, cite files/tests when making code claims, and do not invent project policy.
";

const LEGACY_MARKERS: &[&str] = &[
    "# Shared CodeRoom protocol",
    "Roles are addressed as `@name`",
    "From @backend: <text>",
    "Use `/patch` facts as explicit user-written corrections",
    "Your effective prompt is assembled from shared priors",
];

/// Run CodeRoom project checks and optionally apply exact safe fixes.
pub fn run(project_root: &Path, options: DoctorOptions) -> Result<()> {
    let shared = project_root.join(CODEROOM_DIR).join("shared.md");
    let content = match std::fs::read_to_string(&shared) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("ok: no .coderoom/shared.md found");
            return Ok(());
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", shared.display())),
    };

    match classify_shared(&content) {
        SharedProtocolStatus::Clean => {
            println!("ok: shared.md contains no legacy CodeRoom protocol block");
            Ok(())
        }
        SharedProtocolStatus::LegacyExact | SharedProtocolStatus::LegacyMixed => {
            if options.fix {
                let fixed = fixed_shared(&content).expect("classified exact legacy block");
                std::fs::write(&shared, fixed)
                    .with_context(|| format!("writing {}", shared.display()))?;
                println!(
                    "fixed: removed legacy CodeRoom protocol from {}",
                    shared.display()
                );
            } else {
                println!(
                    "warn: {} contains the old CodeRoom protocol template",
                    shared.display()
                );
                println!("hint: run `cr doctor --fix` to remove the exact legacy block");
            }
            Ok(())
        }
        SharedProtocolStatus::LegacyEdited => bail!(
            "{} appears to contain edited CodeRoom protocol text. Review it manually; \
             doctor only rewrites exact legacy templates.",
            shared.display()
        ),
    }
}

fn classify_shared(content: &str) -> SharedProtocolStatus {
    if content.trim().is_empty() {
        return SharedProtocolStatus::Clean;
    }
    if content.trim() == LEGACY_SHARED_PROTOCOL.trim() {
        return SharedProtocolStatus::LegacyExact;
    }
    if fixed_shared(content).is_some() {
        return SharedProtocolStatus::LegacyMixed;
    }
    let hits = LEGACY_MARKERS
        .iter()
        .filter(|marker| content.contains(**marker))
        .count();
    if hits >= 2 {
        SharedProtocolStatus::LegacyEdited
    } else {
        SharedProtocolStatus::Clean
    }
}

fn fixed_shared(content: &str) -> Option<String> {
    let trimmed_legacy = LEGACY_SHARED_PROTOCOL.trim();
    if content.trim() == trimmed_legacy {
        return Some(PROJECT_SHARED_TEMPLATE.to_owned());
    }
    let pos = content.find(trimmed_legacy)?;
    let mut remaining = String::new();
    remaining.push_str(content[..pos].trim());
    if !remaining.is_empty() {
        remaining.push_str("\n\n");
    }
    remaining.push_str(content[pos + trimmed_legacy.len()..].trim());
    let remaining = remaining.trim();
    if remaining.is_empty() {
        Some(PROJECT_SHARED_TEMPLATE.to_owned())
    } else {
        Some(format!(
            "{}\n\n## Preserved project priors\n\n{}\n",
            PROJECT_SHARED_TEMPLATE.trim_end(),
            remaining
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn shared_project(content: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        fs::create_dir_all(&coderoom).unwrap();
        fs::write(coderoom.join("shared.md"), content).unwrap();
        tmp
    }

    #[test]
    fn clean_shared_is_ok() {
        assert_eq!(
            classify_shared("# Team-wide priors\n\nUse sqlx."),
            SharedProtocolStatus::Clean
        );
    }

    #[test]
    fn exact_legacy_template_is_detected() {
        assert_eq!(
            classify_shared(LEGACY_SHARED_PROTOCOL),
            SharedProtocolStatus::LegacyExact
        );
    }

    #[test]
    fn edited_legacy_protocol_is_not_auto_fixable() {
        let edited = LEGACY_SHARED_PROTOCOL.replace("host role", "router role");
        assert_eq!(classify_shared(&edited), SharedProtocolStatus::LegacyEdited);
        assert!(fixed_shared(&edited).is_none());
    }

    #[test]
    fn fix_exact_legacy_template_with_project_template() {
        let tmp = shared_project(LEGACY_SHARED_PROTOCOL);
        run(tmp.path(), DoctorOptions { fix: true }).unwrap();
        let body = fs::read_to_string(tmp.path().join(CODEROOM_DIR).join("shared.md")).unwrap();
        assert!(body.contains("# Team-wide priors"));
        assert!(!body.contains("# Shared CodeRoom protocol"));
    }

    #[test]
    fn fix_preserves_custom_project_text() {
        let mixed = format!(
            "{}\n\n## Backend standards\n\nUse sqlx.\n",
            LEGACY_SHARED_PROTOCOL.trim_end()
        );
        let tmp = shared_project(&mixed);
        run(tmp.path(), DoctorOptions { fix: true }).unwrap();
        let body = fs::read_to_string(tmp.path().join(CODEROOM_DIR).join("shared.md")).unwrap();
        assert!(body.contains("# Team-wide priors"));
        assert!(body.contains("## Preserved project priors"));
        assert!(body.contains("Use sqlx."));
        assert!(!body.contains("# Shared CodeRoom protocol"));
    }
}
