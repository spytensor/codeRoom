//! Compose a role's full system prompt from on-disk pieces.
//!
//! At spawn time, a role's effective priors are the concatenation of:
//!
//! 1. `.coderoom/shared.md` — cross-role priors (optional)
//! 2. `.coderoom/roles/<role>.md` — base priors for this role
//! 3. `.coderoom/patches/<role>/NNN-*.md` — session-time corrections,
//!    in numeric-prefix order, oldest first. Files starting with `_`
//!    (e.g. `_archive`) are skipped.
//!
//! Each section is separated by a horizontal-rule fence so the role can
//! tell at a glance which knowledge came from where. The composed string
//! is what we hand to the engine via its system-prompt mechanism.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::ROLES_DIR;

/// File name of the cross-role priors document inside `.coderoom/`.
pub const SHARED_FILE: &str = "shared.md";

/// Subdirectory holding per-role correction patches.
pub const PATCHES_DIR: &str = "patches";

/// Section separator inserted between priors sources.
const SECTION_FENCE: &str = "\n\n---\n\n";

/// Compose the full system prompt for `role_name` from `coderoom_dir`.
///
/// Returns an error if the role's base priors file is missing. Optional
/// pieces (shared.md, patches) are silently skipped when absent.
pub fn compose_for(coderoom_dir: &Path, role_name: &str) -> Result<String> {
    let mut out = String::new();

    let shared = coderoom_dir.join(SHARED_FILE);
    if shared.is_file() {
        let content = std::fs::read_to_string(&shared)
            .with_context(|| format!("reading {}", shared.display()))?;
        if !content.trim().is_empty() {
            out.push_str(content.trim_end());
            out.push_str(SECTION_FENCE);
        }
    }

    let role_path = coderoom_dir
        .join(ROLES_DIR)
        .join(format!("{role_name}.md"));
    let role_content = std::fs::read_to_string(&role_path)
        .with_context(|| format!("reading priors for role `{role_name}` at {}", role_path.display()))?;
    out.push_str(role_content.trim_end());

    let patches = ordered_patches(coderoom_dir, role_name)?;
    if !patches.is_empty() {
        out.push_str(SECTION_FENCE);
        out.push_str("## Active patches\n\n");
        for patch in patches {
            let content = std::fs::read_to_string(&patch)
                .with_context(|| format!("reading patch {}", patch.display()))?;
            out.push_str(content.trim_end());
            out.push_str("\n\n");
        }
    }

    out.push('\n');
    Ok(out)
}

/// Return patch files for `role_name`, sorted by their leading numeric
/// prefix (so `001-foo.md` < `002-bar.md` < `010-baz.md`). Files whose
/// names start with `_` are skipped — that prefix is reserved for the
/// `_archive/` overflow bin and any future sentinel files.
fn ordered_patches(coderoom_dir: &Path, role_name: &str) -> Result<Vec<PathBuf>> {
    let dir = coderoom_dir.join(PATCHES_DIR).join(role_name);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<(u32, PathBuf)> = Vec::new();
    for dirent in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let dirent = dirent?;
        let path = dirent.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.starts_with('_') {
            continue;
        }
        if !name.ends_with(".md") {
            continue;
        }
        let leading_digits: String = name.chars().take_while(char::is_ascii_digit).collect();
        let order = leading_digits.parse::<u32>().unwrap_or(u32::MAX);
        entries.push((order, path));
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    Ok(entries.into_iter().map(|(_, p)| p).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CODEROOM_DIR;
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::TempDir;

    /// Create a `.coderoom/` skeleton with a single role's base priors.
    fn fixture(role: &str, role_body: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let coderoom = tmp.path().join(CODEROOM_DIR);
        fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
        fs::write(
            coderoom.join(ROLES_DIR).join(format!("{role}.md")),
            role_body,
        )
        .unwrap();
        tmp
    }

    fn coderoom_of(tmp: &TempDir) -> PathBuf {
        tmp.path().join(CODEROOM_DIR)
    }

    #[test]
    fn role_only_no_optional_pieces() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        let composed = compose_for(&coderoom_of(&tmp), "backend").unwrap();
        // No shared.md present, no patches → just the role body (with trailing newline).
        assert_eq!(composed.trim(), "BACKEND_PRIORS");
    }

    #[test]
    fn shared_then_role_separated_by_fence() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        fs::write(coderoom_of(&tmp).join(SHARED_FILE), "SHARED_PRIORS").unwrap();
        let composed = compose_for(&coderoom_of(&tmp), "backend").unwrap();
        assert!(
            composed.contains("SHARED_PRIORS\n\n---\n\nBACKEND_PRIORS"),
            "composed: {composed:?}"
        );
    }

    #[test]
    fn empty_shared_md_is_skipped() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        fs::write(coderoom_of(&tmp).join(SHARED_FILE), "   \n").unwrap();
        let composed = compose_for(&coderoom_of(&tmp), "backend").unwrap();
        assert!(!composed.contains("---"), "composed should have no fence: {composed:?}");
    }

    #[test]
    fn patches_appear_in_numeric_order() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        let patches_dir = coderoom_of(&tmp).join(PATCHES_DIR).join("backend");
        fs::create_dir_all(&patches_dir).unwrap();
        // Intentionally non-alphabetical creation order: 010 < 002 alphabetically
        // but should sort 002 < 010 numerically.
        fs::write(patches_dir.join("010-late.md"), "PATCH_TEN").unwrap();
        fs::write(patches_dir.join("002-mid.md"), "PATCH_TWO").unwrap();
        fs::write(patches_dir.join("001-first.md"), "PATCH_ONE").unwrap();

        let composed = compose_for(&coderoom_of(&tmp), "backend").unwrap();
        let one = composed.find("PATCH_ONE").expect("contains PATCH_ONE");
        let two = composed.find("PATCH_TWO").expect("contains PATCH_TWO");
        let ten = composed.find("PATCH_TEN").expect("contains PATCH_TEN");
        assert!(one < two, "PATCH_ONE before PATCH_TWO");
        assert!(two < ten, "PATCH_TWO before PATCH_TEN");
    }

    #[test]
    fn underscored_files_are_skipped() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        let patches_dir = coderoom_of(&tmp).join(PATCHES_DIR).join("backend");
        fs::create_dir_all(&patches_dir).unwrap();
        fs::write(patches_dir.join("001-keep.md"), "KEEP").unwrap();
        fs::write(patches_dir.join("_archive_note.md"), "DROP_ARCHIVE").unwrap();
        fs::write(patches_dir.join("not-a-patch.txt"), "DROP_TXT").unwrap();

        let composed = compose_for(&coderoom_of(&tmp), "backend").unwrap();
        assert!(composed.contains("KEEP"));
        assert!(!composed.contains("DROP_ARCHIVE"));
        assert!(!composed.contains("DROP_TXT"));
    }

    #[test]
    fn missing_role_priors_errors_with_path_in_message() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        let err = compose_for(&coderoom_of(&tmp), "frontend").expect_err("missing role");
        let msg = format!("{err:#}");
        assert!(msg.contains("frontend"), "error mentions role name: {msg}");
    }

    #[test]
    fn header_appears_only_when_patches_present() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        // No patches dir at all.
        let composed = compose_for(&coderoom_of(&tmp), "backend").unwrap();
        assert!(!composed.contains("Active patches"));

        // Empty patches dir present but no patch files.
        let patches_dir = coderoom_of(&tmp).join(PATCHES_DIR).join("backend");
        fs::create_dir_all(&patches_dir).unwrap();
        let composed = compose_for(&coderoom_of(&tmp), "backend").unwrap();
        assert!(!composed.contains("Active patches"));
    }

    #[test]
    fn composition_ends_with_single_trailing_newline() {
        let tmp = fixture("backend", "BACKEND_PRIORS\n\n\n");
        let composed = compose_for(&coderoom_of(&tmp), "backend").unwrap();
        assert!(composed.ends_with('\n'));
        assert!(!composed.ends_with("\n\n"));
    }
}
