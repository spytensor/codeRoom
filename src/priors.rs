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
use serde::Deserialize;

use crate::config::{CONFIG_FILE, ROLES_DIR};

/// Hard cap on active patches per role at v0.1. Once exceeded, the
/// oldest patch is moved to `_archive/` (still loadable on demand,
/// just not auto-loaded) so the active set never silently grows
/// without bound. Documented in `docs/architecture.md` as a v0.1
/// invariant — change with care.
pub const MAX_ACTIVE_PATCHES_PER_ROLE: usize = 50;

/// Subdirectory under each role's `patches/<role>/` that holds patches
/// evicted by the FIFO cap. Files here are NOT auto-loaded into a
/// role's priors; they exist for forensics / opt-in re-promotion.
pub const ARCHIVE_SUBDIR: &str = "_archive";

/// Subdirectory of `.coderoom/` that holds per-day, per-role journals.
pub const JOURNAL_DIR: &str = "journal";

/// Number of days of journal entries auto-loaded into a role's priors
/// at spawn time. Anything older still lives on disk (grep-able) but
/// stays out of the composed system prompt.
pub const JOURNAL_WINDOW_DAYS: i64 = 7;

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

    let role_path = coderoom_dir.join(ROLES_DIR).join(format!("{role_name}.md"));
    let role_content = std::fs::read_to_string(&role_path).with_context(|| {
        format!(
            "reading priors for role `{role_name}` at {}",
            role_path.display()
        )
    })?;
    out.push_str(role_content.trim_end());

    let roster = team_roster(coderoom_dir, role_name)?;
    if !roster.is_empty() {
        out.push_str(SECTION_FENCE);
        out.push_str("## Team roster\n\n");
        out.push_str(&roster);
    }

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

    let journals = recent_journals(coderoom_dir, role_name, JOURNAL_WINDOW_DAYS)?;
    if !journals.is_empty() {
        out.push_str(SECTION_FENCE);
        out.push_str("## Recent journal entries\n\n");
        for (date, path) in journals {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("reading journal {}", path.display()))?;
            let trimmed = content.trim();
            if trimmed.is_empty() {
                continue;
            }
            out.push_str(&format!("### {date}\n\n"));
            out.push_str(trimmed);
            out.push_str("\n\n");
        }
    }

    out.push('\n');
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct RosterConfig {
    host_role: String,
}

fn team_roster(coderoom_dir: &Path, role_name: &str) -> Result<String> {
    let roles_dir = coderoom_dir.join(ROLES_DIR);
    if !roles_dir.is_dir() {
        return Ok(String::new());
    }
    let host_role = read_host_role(coderoom_dir).unwrap_or_else(|| "host".to_owned());
    let mut roles = std::fs::read_dir(&roles_dir)
        .with_context(|| format!("reading {}", roles_dir.display()))?
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_owned();
            (name != role_name).then_some((name, path))
        })
        .collect::<Vec<_>>();
    roles.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::new();
    for (name, path) in roles {
        let summary = role_summary(&path)?;
        let host_marker = if name == host_role { " (host)" } else { "" };
        out.push_str(&format!("- @{name}{host_marker}: {summary}\n"));
    }
    Ok(out.trim_end().to_owned())
}

fn read_host_role(coderoom_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(coderoom_dir.join(CONFIG_FILE)).ok()?;
    toml::from_str::<RosterConfig>(&text)
        .ok()
        .map(|cfg| cfg.host_role)
}

fn role_summary(path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading role priors {}", path.display()))?;
    let summary = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or("configured peer role");
    Ok(truncate_summary(summary, 160))
}

fn truncate_summary(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_owned();
    }
    let mut out = input
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

/// Return `(date, path)` pairs for the role's journal entries within the
/// last `window_days`, oldest-first. The on-disk layout is
/// `.coderoom/journal/YYYY-MM-DD/<role>.md`; only directories whose name
/// parses as a date and that contain a matching role file are included.
///
/// Outside the window, files are skipped (still on disk, still grep-able).
pub fn recent_journals(
    coderoom_dir: &Path,
    role_name: &str,
    window_days: i64,
) -> Result<Vec<(String, PathBuf)>> {
    let dir = coderoom_dir.join(JOURNAL_DIR);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let today = chrono::Local::now().date_naive();
    let cutoff = today
        .checked_sub_signed(chrono::Duration::days(window_days))
        .unwrap_or(today);

    let mut entries: Vec<(chrono::NaiveDate, String, PathBuf)> = Vec::new();
    for dirent in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let dirent = dirent?;
        let day_path = dirent.path();
        if !day_path.is_dir() {
            continue;
        }
        let Some(name) = day_path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(date) = chrono::NaiveDate::parse_from_str(name, "%Y-%m-%d") else {
            continue;
        };
        if date < cutoff || date > today {
            continue;
        }
        let role_file = day_path.join(format!("{role_name}.md"));
        if role_file.is_file() {
            entries.push((date, name.to_owned(), role_file));
        }
    }

    entries.sort_by_key(|(date, _, _)| *date);
    Ok(entries
        .into_iter()
        .map(|(_, label, path)| (label, path))
        .collect())
}

/// Outcome of a [`write_patch`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchWriteOutcome {
    /// Path of the file the new patch was written to.
    pub path: PathBuf,
    /// If the FIFO cap was exceeded, the path moved to `_archive/`.
    pub archived: Option<PathBuf>,
}

/// Persist a new correction for `role_name` under
/// `.coderoom/patches/<role>/NNN-<slug>.md`.
///
/// Sequence number is `1 + max(prefix)` across both the active patches
/// directory AND its `_archive/` subdir, so numbers stay monotonic
/// across archival. `slug` is derived from the first ~40 chars of
/// `text` (ASCII-lowercased, non-alphanumeric collapsed to dashes).
///
/// After writing, enforces [`MAX_ACTIVE_PATCHES_PER_ROLE`] — the oldest
/// active patch (lowest sequence number) is moved into `_archive/`.
pub fn write_patch(coderoom_dir: &Path, role_name: &str, text: &str) -> Result<PatchWriteOutcome> {
    let dir = coderoom_dir.join(PATCHES_DIR).join(role_name);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let next_seq = next_patch_seq(&dir)?;
    let slug = slugify(text);
    let filename = format!("{next_seq:03}-{slug}.md");
    let path = dir.join(&filename);

    let trimmed = text.trim();
    let body = if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    };
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;

    let archived = enforce_active_cap(&dir)?;
    Ok(PatchWriteOutcome { path, archived })
}

/// Compact archived patches and old journal entries into the role's
/// base priors file. This is deterministic and local: it preserves
/// source paths and short excerpts instead of asking an engine to
/// summarize itself.
pub fn compact_role(coderoom_dir: &Path, role_name: &str) -> Result<PathBuf> {
    let role_path = coderoom_dir.join(ROLES_DIR).join(format!("{role_name}.md"));
    let mut body = std::fs::read_to_string(&role_path)
        .with_context(|| format!("reading role priors {}", role_path.display()))?;
    let summary = compact_summary(coderoom_dir, role_name)?;
    if summary.trim().is_empty() {
        return Ok(role_path);
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str("\n## Compacted history\n\n");
    body.push_str(&summary);
    body.push('\n');
    std::fs::write(&role_path, body)
        .with_context(|| format!("writing compacted priors {}", role_path.display()))?;
    Ok(role_path)
}

fn compact_summary(coderoom_dir: &Path, role_name: &str) -> Result<String> {
    let mut out = String::new();
    let archive = coderoom_dir
        .join(PATCHES_DIR)
        .join(role_name)
        .join(ARCHIVE_SUBDIR);
    if archive.is_dir() {
        let mut patches = std::fs::read_dir(&archive)
            .with_context(|| format!("reading {}", archive.display()))?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("md"))
            .collect::<Vec<_>>();
        patches.sort();
        for path in patches {
            let excerpt = first_content_line(&path)?;
            out.push_str(&format!(
                "- Archived patch `{}`: {}\n",
                path.display(),
                excerpt
            ));
        }
    }

    let journals = old_journals(coderoom_dir, role_name, JOURNAL_WINDOW_DAYS)?;
    for (date, path) in journals {
        let excerpt = first_content_line(&path)?;
        out.push_str(&format!(
            "- Journal {date} `{}`: {}\n",
            path.display(),
            excerpt
        ));
    }
    Ok(out)
}

fn first_content_line(path: &Path) -> Result<String> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let line = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or("(empty)");
    Ok(truncate_summary(line, 180))
}

fn old_journals(
    coderoom_dir: &Path,
    role_name: &str,
    window_days: i64,
) -> Result<Vec<(String, PathBuf)>> {
    let dir = coderoom_dir.join(JOURNAL_DIR);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let today = chrono::Local::now().date_naive();
    let cutoff = today
        .checked_sub_signed(chrono::Duration::days(window_days))
        .unwrap_or(today);
    let mut entries = Vec::new();
    for dirent in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let day_path = dirent?.path();
        let Some(name) = day_path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(date) = chrono::NaiveDate::parse_from_str(name, "%Y-%m-%d") else {
            continue;
        };
        if date >= cutoff {
            continue;
        }
        let role_file = day_path.join(format!("{role_name}.md"));
        if role_file.is_file() {
            entries.push((date, name.to_owned(), role_file));
        }
    }
    entries.sort_by_key(|(date, _, _)| *date);
    Ok(entries
        .into_iter()
        .map(|(_, label, path)| (label, path))
        .collect())
}

/// Pick the next patch sequence number for `dir`. Considers both the
/// directory's active patch files and its `_archive/` subdirectory so
/// numbers never collide post-archival. Returns 1 for an empty role.
fn next_patch_seq(dir: &Path) -> Result<u32> {
    let mut max = 0u32;
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            if let Some(seq) = leading_seq_from_md(&path) {
                max = max.max(seq);
            }
        }
    }
    let archive = dir.join(ARCHIVE_SUBDIR);
    if archive.is_dir() {
        for entry in
            std::fs::read_dir(&archive).with_context(|| format!("reading {}", archive.display()))?
        {
            let path = entry?.path();
            if let Some(seq) = leading_seq_from_md(&path) {
                max = max.max(seq);
            }
        }
    }
    Ok(max + 1)
}

fn leading_seq_from_md(path: &Path) -> Option<u32> {
    if path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
        != Some("md")
    {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    if name.starts_with('_') {
        return None;
    }
    let leading: String = name.chars().take_while(char::is_ascii_digit).collect();
    leading.parse::<u32>().ok()
}

/// If the active patches directory has more than [`MAX_ACTIVE_PATCHES_PER_ROLE`]
/// entries, move the oldest (lowest-sequence) into `_archive/`.
fn enforce_active_cap(dir: &Path) -> Result<Option<PathBuf>> {
    let mut active: Vec<(u32, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        if let Some(seq) = leading_seq_from_md(&path) {
            active.push((seq, path));
        }
    }
    if active.len() <= MAX_ACTIVE_PATCHES_PER_ROLE {
        return Ok(None);
    }
    active.sort_by_key(|(seq, _)| *seq);
    let (_, oldest) = active
        .into_iter()
        .next()
        .expect("non-empty after cap check");

    let archive = dir.join(ARCHIVE_SUBDIR);
    std::fs::create_dir_all(&archive).with_context(|| format!("creating {}", archive.display()))?;
    let dest = archive.join(oldest.file_name().expect("file_name on existing path"));
    std::fs::rename(&oldest, &dest)
        .with_context(|| format!("archiving {} → {}", oldest.display(), dest.display()))?;
    Ok(Some(dest))
}

/// Lowercase, ASCII-only, dash-separated slug. Empty input → `"untitled"`.
/// Truncated to ~40 chars at a word boundary so filenames stay readable.
fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(40);
    let mut last_dash = false;
    for c in text.chars() {
        if out.len() >= 40 {
            break;
        }
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("untitled");
    }
    out
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
        if path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
            != Some("md")
        {
            continue;
        }
        let leading_digits: String = name.chars().take_while(char::is_ascii_digit).collect();
        let order = leading_digits.parse::<u32>().unwrap_or(u32::MAX);
        entries.push((order, path));
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    Ok(entries.into_iter().map(|(_, p)| p).collect())
}

/// Cheap upper-bound estimate of how many tokens a role's composed
/// priors will burn at spawn. Sums byte counts of the role's priors
/// file plus `shared.md`, divides by 4 (rough chars-per-token for
/// English markdown). Doesn't compose patches/journal — those are
/// transient and small. Only used for splash display, so accuracy
/// to within ±20% is fine.
#[must_use]
pub fn estimate_role_tokens(coderoom_dir: &Path, role_name: &str) -> u64 {
    let role_path = coderoom_dir.join(ROLES_DIR).join(format!("{role_name}.md"));
    let role_bytes = std::fs::metadata(&role_path).map(|m| m.len()).unwrap_or(0);
    let shared_bytes = std::fs::metadata(coderoom_dir.join(SHARED_FILE))
        .map(|m| m.len())
        .unwrap_or(0);
    (role_bytes + shared_bytes) / 4
}

/// Format a token count as `"3.2k"` for ≥1000, otherwise the bare
/// number. Used by both the welcome splash and the steady-state
/// single-line summary.
#[must_use]
pub fn format_token_count(n: u64) -> String {
    if n >= 1_000 {
        // Casting u64 → f64 cannot lose precision for any realistic
        // priors size (well under 2^53).
        #[allow(clippy::cast_precision_loss)]
        let kilos = n as f64 / 1000.0;
        format!("{kilos:.1}k")
    } else {
        n.to_string()
    }
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
    fn compose_includes_team_roster_for_peer_roles() {
        let tmp = fixture("backend", "# Backend\n\nOwns APIs.");
        let coderoom = coderoom_of(&tmp);
        fs::write(coderoom.join(CONFIG_FILE), "host_role = \"host\"\n").unwrap();
        fs::write(
            coderoom.join(ROLES_DIR).join("host.md"),
            "# Host\n\nRoutes work.",
        )
        .unwrap();
        fs::write(
            coderoom.join(ROLES_DIR).join("security.md"),
            "# Security\n\nReviews risk.",
        )
        .unwrap();

        let composed = compose_for(&coderoom, "backend").unwrap();
        assert!(composed.contains("## Team roster"));
        assert!(composed.contains("@host (host): Routes work."));
        assert!(composed.contains("@security: Reviews risk."));
        assert!(!composed.contains("@backend:"));
    }

    #[test]
    fn empty_shared_md_is_skipped() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        fs::write(coderoom_of(&tmp).join(SHARED_FILE), "   \n").unwrap();
        let composed = compose_for(&coderoom_of(&tmp), "backend").unwrap();
        assert!(
            !composed.contains("---"),
            "composed should have no fence: {composed:?}"
        );
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

    #[test]
    fn slugify_basic_cases() {
        assert_eq!(
            slugify("Use verify_token() instead"),
            "use-verify-token-instead"
        );
        assert_eq!(slugify(""), "untitled");
        assert_eq!(slugify("!!!"), "untitled");
        assert_eq!(slugify("rate-limit in gateway"), "rate-limit-in-gateway");
        assert!(
            slugify("a really really really long description that goes on and on and on").len()
                <= 40
        );
    }

    #[test]
    fn write_patch_creates_numbered_file() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        let coderoom = coderoom_of(&tmp);
        let outcome = write_patch(&coderoom, "backend", "use verify_token()").unwrap();

        assert!(outcome.path.exists());
        assert!(outcome.archived.is_none());
        let name = outcome.path.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("001-"),
            "first patch should be sequence 001: {name}"
        );
        assert_eq!(
            outcome.path.extension().and_then(|e| e.to_str()),
            Some("md")
        );
        let content = fs::read_to_string(&outcome.path).unwrap();
        assert!(content.contains("verify_token"));
    }

    #[test]
    fn write_patch_increments_across_archived_entries() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        let coderoom = coderoom_of(&tmp);
        let dir = coderoom.join(PATCHES_DIR).join("backend");
        let archive = dir.join(ARCHIVE_SUBDIR);
        fs::create_dir_all(&archive).unwrap();
        // Pretend 003 was already archived.
        fs::write(archive.join("003-old.md"), "OLD").unwrap();

        let outcome = write_patch(&coderoom, "backend", "fresh patch").unwrap();
        let name = outcome.path.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("004-"),
            "next seq should be 004 (max 003 + 1): {name}"
        );
    }

    #[test]
    fn write_patch_enforces_fifo_cap() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        let coderoom = coderoom_of(&tmp);

        // Write MAX patches; none should be archived yet.
        for i in 0..MAX_ACTIVE_PATCHES_PER_ROLE {
            let out = write_patch(&coderoom, "backend", &format!("patch {i}")).unwrap();
            assert!(
                out.archived.is_none(),
                "iteration {i} unexpectedly archived"
            );
        }

        // The (MAX+1)-th write should evict the oldest active entry.
        let out = write_patch(&coderoom, "backend", "overflow").unwrap();
        let archived = out.archived.expect("expected archival on overflow");
        let name = archived.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("001-"),
            "FIFO should evict sequence 001 first: {name}"
        );
        assert!(archived.starts_with(
            coderoom
                .join(PATCHES_DIR)
                .join("backend")
                .join(ARCHIVE_SUBDIR)
        ));

        // Active dir should now hold exactly MAX entries again.
        let dir = coderoom.join(PATCHES_DIR).join("backend");
        let active_count = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().is_file() && leading_seq_from_md(&e.path()).is_some())
            .count();
        assert_eq!(active_count, MAX_ACTIVE_PATCHES_PER_ROLE);
    }

    #[test]
    fn compact_role_appends_archived_patch_and_old_journal_summary() {
        let tmp = fixture("backend", "BACKEND_PRIORS\n");
        let coderoom = coderoom_of(&tmp);
        let archive = coderoom
            .join(PATCHES_DIR)
            .join("backend")
            .join(ARCHIVE_SUBDIR);
        fs::create_dir_all(&archive).unwrap();
        fs::write(archive.join("001-old.md"), "Use the gateway path.\n").unwrap();

        let old_day = chrono::Local::now()
            .date_naive()
            .checked_sub_signed(chrono::Duration::days(JOURNAL_WINDOW_DAYS + 2))
            .unwrap()
            .format("%Y-%m-%d")
            .to_string();
        let old_dir = coderoom.join(JOURNAL_DIR).join(&old_day);
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(
            old_dir.join("backend.md"),
            "Decision with src/lib.rs evidence.\n",
        )
        .unwrap();

        let path = compact_role(&coderoom, "backend").unwrap();
        let body = fs::read_to_string(path).unwrap();
        assert!(body.contains("## Compacted history"));
        assert!(body.contains("Archived patch"));
        assert!(body.contains("Use the gateway path."));
        assert!(body.contains(&old_day));
        assert!(body.contains("Decision with src/lib.rs evidence."));
    }

    #[test]
    fn next_patch_seq_starts_at_1_for_empty() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("patches/backend");
        // No directory at all — should still yield 1.
        assert_eq!(next_patch_seq(&dir).unwrap(), 1);
    }

    #[test]
    fn recent_journals_includes_today_and_recent_days() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        let coderoom = coderoom_of(&tmp);
        let today = chrono::Local::now().date_naive();
        let yesterday = today - chrono::Duration::days(1);
        let too_old = today - chrono::Duration::days(30);

        for date in [today, yesterday, too_old] {
            let dir = coderoom
                .join(JOURNAL_DIR)
                .join(date.format("%Y-%m-%d").to_string());
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("backend.md"), format!("entry for {date}")).unwrap();
        }

        let entries = recent_journals(&coderoom, "backend", 7).unwrap();
        assert_eq!(entries.len(), 2, "today + yesterday only");
        // chronological order: yesterday first, then today.
        assert_eq!(entries[0].0, yesterday.format("%Y-%m-%d").to_string());
        assert_eq!(entries[1].0, today.format("%Y-%m-%d").to_string());
    }

    #[test]
    fn recent_journals_filters_by_role() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        let coderoom = coderoom_of(&tmp);
        let today = chrono::Local::now().date_naive();
        let dir = coderoom
            .join(JOURNAL_DIR)
            .join(today.format("%Y-%m-%d").to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("backend.md"), "BE").unwrap();
        fs::write(dir.join("frontend.md"), "FE").unwrap();

        let backend = recent_journals(&coderoom, "backend", 7).unwrap();
        assert_eq!(backend.len(), 1);
        assert!(backend[0].1.ends_with("backend.md"));
    }

    #[test]
    fn recent_journals_handles_missing_dir() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        let coderoom = coderoom_of(&tmp);
        let entries = recent_journals(&coderoom, "backend", 7).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn format_token_count_uses_one_decimal_kilo_suffix() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1_000), "1.0k");
        assert_eq!(format_token_count(3_240), "3.2k");
        assert_eq!(format_token_count(21_500), "21.5k");
    }

    #[test]
    fn estimate_role_tokens_reads_role_plus_shared() {
        let tmp = fixture("backend", &"x".repeat(4_000));
        let coderoom = coderoom_of(&tmp);
        // No shared.md yet → ~4000 bytes / 4 = 1000 tokens
        assert_eq!(estimate_role_tokens(&coderoom, "backend"), 1_000);

        fs::write(coderoom.join(SHARED_FILE), "y".repeat(8_000)).unwrap();
        // (4000 + 8000) / 4 = 3000
        assert_eq!(estimate_role_tokens(&coderoom, "backend"), 3_000);
    }

    #[test]
    fn estimate_role_tokens_returns_zero_for_unknown_role() {
        let tmp = fixture("backend", "x");
        let coderoom = coderoom_of(&tmp);
        // unknown role → no role.md → contributes 0; shared.md missing too.
        assert_eq!(estimate_role_tokens(&coderoom, "ghost"), 0);
    }

    #[test]
    fn compose_includes_recent_journal_section() {
        let tmp = fixture("backend", "BACKEND_PRIORS");
        let coderoom = coderoom_of(&tmp);
        let today = chrono::Local::now().date_naive();
        let dir = coderoom
            .join(JOURNAL_DIR)
            .join(today.format("%Y-%m-%d").to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("backend.md"), "JOURNAL_TODAY").unwrap();

        let composed = compose_for(&coderoom, "backend").unwrap();
        assert!(composed.contains("Recent journal"));
        assert!(composed.contains("JOURNAL_TODAY"));
    }
}
