//! Git-backed pointers inside role priors.
//!
//! ## Why
//!
//! A role's priors file used to say "watch `src/adapter/cc.rs`" in
//! prose. The model would either re-read that file via tool calls
//! every turn (waste) or operate on whatever stale snippet the priors
//! had inlined (lie). Pointers are the structural fix: a priors
//! file references *a specific commit and line range* of *a specific
//! path*, and at session-spawn time we resolve those references to
//! fresh content. When the locked SHA falls behind HEAD, we flag the
//! pointer as stale so the role knows its priors describe an older
//! reality than the working tree.
//!
//! See `docs/proposed-amendments.md` and the project thesis memory
//! for why this is the v0.5 foundation rather than a polish item:
//! every later evolution / contract / inbox primitive depends on
//! "facts have provenance" and "facts can become stale."
//!
//! ## Token grammar
//!
//! ```text
//! [[<path>#L<n>-<m>@<sha>]]   locked range
//! [[<path>#L<n>@<sha>]]        locked single line
//! [[<path>@<sha>]]              locked whole file
//! [[<path>#L<n>-<m>]]           HEAD range (no staleness check)
//! [[<path>#L<n>]]               HEAD single line
//! [[<path>@HEAD]]                HEAD whole file (explicit form)
//! ```
//!
//! **Every pointer must carry at least one anchor** — either a
//! `#L<range>` line specifier or an `@<sha>` / `@HEAD` suffix. A
//! wholly-unanchored `[[bare-word]]` is rejected at parse time so
//! `[[TODO]]` or `[[FIXME]]` in prose never silently triggers a
//! filesystem read. The HEAD-tracking branch is also container-
//! checked: a path that canonicalises outside `repo_root` (e.g.
//! `[[../../etc/passwd@HEAD]]`) is rejected at resolve time.
//!
//! `<sha>` may be a short or full hex object id; git resolves both.
//! `@HEAD` means "follow the working tree, no staleness check" —
//! useful for paths the role wants bleeding-edge (e.g.
//! `[[Cargo.toml@HEAD]]`), at the cost of the drift signal.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;

/// One pointer parsed out of priors. Cheap to clone; the heavy work
/// is in [`resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pointer {
    /// Repo-relative path.
    pub path: PathBuf,
    /// Inclusive 1-based line range. `None` means whole-file.
    pub line_range: Option<(u32, u32)>,
    /// Git object id the pointer is locked to. `None` means "HEAD,
    /// no staleness check" — the escape hatch.
    pub locked_sha: Option<String>,
    /// The raw `[[...]]` token from the priors text, kept verbatim so
    /// resolved output can quote the original reference.
    pub raw: String,
}

/// Result of one pointer resolution. The renderer turns this into a
/// markdown section the model can read.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The pointer that was resolved.
    pub pointer: Pointer,
    /// Outcome of the resolution attempt.
    pub status: PointerStatus,
    /// File content at the locked SHA (or HEAD if no SHA), narrowed
    /// to `line_range` when set. `None` only when [`Self::status`] is
    /// `Unresolvable`.
    pub content: Option<String>,
}

/// Resolution outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerStatus {
    /// Fresh: pointer has no SHA (intentional HEAD-tracking) or the
    /// locked SHA is the current HEAD.
    Fresh,
    /// SHA exists in history but HEAD has advanced past it. Content
    /// returned is from the locked SHA, not HEAD — staying faithful
    /// to what the priors author saw when they wrote the reference.
    Stale {
        /// Current HEAD object id (short form).
        head_sha: String,
    },
    /// Pointer could not be resolved at all (path missing at SHA,
    /// SHA unknown to git, not in a repo, etc.). `content` is `None`.
    Unresolvable {
        /// Human-readable explanation for the priors render and for
        /// `cr show pointers`.
        reason: String,
    },
}

static TOKEN_RE: OnceLock<Regex> = OnceLock::new();

fn token_re() -> &'static Regex {
    TOKEN_RE.get_or_init(|| {
        // `[[` + non-bracket / non-whitespace body + `]]`. Body can
        // legally contain `/` `.` `_` `-` `#` `L` `@` and hex digits,
        // but rather than enumerate we accept any non-bracket / non-
        // whitespace run and validate inside [`parse_token`].
        Regex::new(r"\[\[([^\[\]\s]+)\]\]").expect("static regex compiles")
    })
}

/// Parse a token body like `src/foo.rs#L42-67@a1b2c3` into a
/// [`Pointer`]. Returns `None` for malformed tokens AND for tokens
/// that lack any anchor signal — `[[bare-word]]` is rejected so
/// stray markdown-link-looking prose can't accidentally trigger a
/// file read. A pointer must carry **at least one** of `#L<range>`
/// or `@<hex-sha>` to be accepted.
///
/// The literal suffix `@HEAD` is also accepted as "no SHA lock,
/// follow HEAD" so users typing the obvious form don't get a token
/// that silently parses with `HEAD` as part of the path.
#[must_use]
pub fn parse_token(raw_token: &str) -> Option<Pointer> {
    // Pull off `@<sha>` suffix. Three cases:
    //   `@<hex>`  → locked SHA
    //   `@HEAD`   → explicit head-tracking (still counts as an anchor)
    //   `@<other>` → not a SHA; treat as part of the path
    let (left, locked_sha, had_at_anchor) = match raw_token.rsplit_once('@') {
        Some((left, tail)) if is_hex_sha(tail) => (left, Some(tail.to_owned()), true),
        Some((left, "HEAD")) => (left, None, true),
        _ => (raw_token, None, false),
    };
    // Pull off `#L<n>` or `#L<n>-<m>` suffix.
    let (path_str, line_range, had_line_anchor) = if let Some((p, anchor)) = left.rsplit_once('#') {
        match parse_line_anchor(anchor) {
            Some(range) => (p, Some(range), true),
            None => return None,
        }
    } else {
        (left, None, false)
    };
    if path_str.is_empty() {
        return None;
    }
    // Require at least one anchor — kills `[[TODO]]` / `[[FIXME]]`
    // / any random `[[word]]` in prose from triggering a file read.
    // See review feedback S1 + B1 (path traversal): a bare word that
    // happens to be `..` or `/etc/passwd` is too dangerous a default.
    if !had_at_anchor && !had_line_anchor {
        return None;
    }
    Some(Pointer {
        path: PathBuf::from(path_str),
        line_range,
        locked_sha,
        raw: format!("[[{raw_token}]]"),
    })
}

fn parse_line_anchor(anchor: &str) -> Option<(u32, u32)> {
    // Anchor must start with `L`.
    let digits = anchor.strip_prefix('L')?;
    if let Some((start, end)) = digits.split_once('-') {
        let start: u32 = start.parse().ok()?;
        let end: u32 = end.parse().ok()?;
        if start == 0 || end < start {
            return None;
        }
        Some((start, end))
    } else {
        let n: u32 = digits.parse().ok()?;
        if n == 0 {
            return None;
        }
        Some((n, n))
    }
}

fn is_hex_sha(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Resolve `pointer` against the git repo rooted at `repo_root`.
/// Reads from the locked SHA when present; from the working tree
/// otherwise. Never panics on missing files / unknown SHAs — those
/// become [`PointerStatus::Unresolvable`].
///
/// For batched resolution use [`expand_text`] / [`print_role_pointers`]
/// — those resolve HEAD once and reuse the result across every
/// pointer in the priors file, instead of forking `git` 3–4× per
/// pointer.
#[must_use]
pub fn resolve(pointer: &Pointer, repo_root: &Path) -> Resolved {
    let head = resolve_head(repo_root);
    resolve_with_head(pointer, repo_root, head.as_deref())
}

/// One-shot HEAD lookup used as a shared cache across batched
/// pointer resolutions. Returns `None` when the path isn't a git
/// repo at all; downstream resolvers turn that into a clean
/// `Unresolvable` reason per pointer.
fn resolve_head(repo_root: &Path) -> Option<String> {
    run_git(repo_root, &["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_owned())
}

fn resolve_with_head(pointer: &Pointer, repo_root: &Path, head_full: Option<&str>) -> Resolved {
    let status = staleness(pointer, repo_root, head_full);
    let (content, status) = match &status {
        PointerStatus::Unresolvable { .. } => (None, status.clone()),
        PointerStatus::Fresh | PointerStatus::Stale { .. } => match read_at(pointer, repo_root) {
            Ok(text) => (Some(text), status),
            Err(error) => (
                None,
                PointerStatus::Unresolvable {
                    reason: error.to_string(),
                },
            ),
        },
    };
    Resolved {
        pointer: pointer.clone(),
        status,
        content,
    }
}

fn staleness(pointer: &Pointer, repo_root: &Path, head_full: Option<&str>) -> PointerStatus {
    let Some(locked_sha) = pointer.locked_sha.as_deref() else {
        return PointerStatus::Fresh;
    };
    let Some(head_full) = head_full else {
        return PointerStatus::Unresolvable {
            reason: "could not resolve HEAD (not a git repo?)".to_owned(),
        };
    };
    // Resolve the locked sha to its full form. If git can't, the
    // priors reference a commit that doesn't exist in this clone
    // (e.g. a force-push discarded it). Mark unresolvable explicitly
    // — different from "stale" because user action is needed.
    let locked_full = match run_git(repo_root, &["rev-parse", locked_sha]) {
        Ok(s) => s.trim().to_owned(),
        Err(_) => {
            return PointerStatus::Unresolvable {
                reason: format!("locked sha `{locked_sha}` is not in this repo"),
            };
        }
    };
    if locked_full == head_full {
        return PointerStatus::Fresh;
    }
    // Short HEAD form for the user-facing message — full SHAs are
    // too long to scan visually in an 80-col terminal.
    let head_short: String = head_full.chars().take(8).collect();
    PointerStatus::Stale {
        head_sha: head_short,
    }
}

fn read_at(pointer: &Pointer, repo_root: &Path) -> Result<String> {
    let raw = if let Some(sha) = pointer.locked_sha.as_deref() {
        // `git show <sha>:<path>` is safe by construction — git
        // explicitly rejects `../` and absolute paths with "path
        // outside repository". Argv (not shell) so no metacharacter
        // escape concerns either.
        run_git(
            repo_root,
            &["show", &format!("{sha}:{}", path_for_git(&pointer.path))],
        )?
    } else {
        // HEAD-tracking branch: read directly from the working tree
        // so the role sees uncommitted edits. We must do our own
        // containment check here since git isn't gating it. A priors
        // file pasted from elsewhere might carry `[[../../etc/passwd]]`
        // or `[[/etc/passwd]]` — without this, every role spawn would
        // exfiltrate the contents into `messages.jsonl` and the
        // engine's context.
        let full = repo_root.join(&pointer.path);
        let canonical_root = repo_root
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("canonicalising repo root: {e}"))?;
        let canonical_full = full
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("canonicalising {}: {e}", full.display()))?;
        if !canonical_full.starts_with(&canonical_root) {
            anyhow::bail!(
                "path {} escapes repo root {}",
                canonical_full.display(),
                canonical_root.display(),
            );
        }
        std::fs::read_to_string(&canonical_full)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", canonical_full.display()))?
    };
    Ok(narrow_to_range(&raw, pointer.line_range))
}

fn narrow_to_range(text: &str, range: Option<(u32, u32)>) -> String {
    let Some((start, end)) = range else {
        return text.to_owned();
    };
    // Inclusive 1-based.
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let n = u32::try_from(i).unwrap_or(u32::MAX).saturating_add(1);
            if n >= start && n <= end {
                Some(line)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Path joined with forward slashes — git uses Unix path separators
/// regardless of platform. `Path::display()` would emit `\` on
/// Windows and confuse `git show`.
fn path_for_git(path: &Path) -> String {
    path.iter()
        .filter_map(|p| p.to_str())
        .collect::<Vec<_>>()
        .join("/")
}

fn run_git(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("running git: {e}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        anyhow::bail!("git {} failed: {err}", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Expand every `[[...]]` pointer token in `text` into a verbose
/// markdown section showing path, locked SHA, status, and the
/// resolved file content. Unparseable tokens (e.g. `[[example]]`
/// in prose) pass through unchanged. Called by `priors::compose_for`.
#[must_use]
pub fn expand_text(text: &str, repo_root: &Path) -> String {
    let re = token_re();
    // Resolve HEAD once per pass so a priors file with N pointers
    // doesn't fork `git rev-parse HEAD` N times. The cost is dominated
    // by `git show` per pointer; everything else folds into one call.
    let head = resolve_head(repo_root);
    let head_ref = head.as_deref();
    let mut out = String::with_capacity(text.len());
    let mut last_end = 0usize;
    for caps in re.captures_iter(text) {
        let m = caps.get(0).expect("regex has at least one group");
        out.push_str(&text[last_end..m.start()]);
        let body = caps.get(1).expect("group 1 matches").as_str();
        match parse_token(body) {
            Some(pointer) => {
                let resolved = resolve_with_head(&pointer, repo_root, head_ref);
                render_resolved(&mut out, &resolved);
            }
            None => out.push_str(m.as_str()),
        }
        last_end = m.end();
    }
    out.push_str(&text[last_end..]);
    out
}

fn render_resolved(out: &mut String, r: &Resolved) {
    use std::fmt::Write as _;
    let p = &r.pointer;
    let range = match p.line_range {
        Some((s, e)) if s == e => format!("#L{s}"),
        Some((s, e)) => format!("#L{s}-{e}"),
        None => String::new(),
    };
    let sha = p.locked_sha.as_deref().unwrap_or("HEAD");
    let _ = writeln!(out);
    match &r.status {
        PointerStatus::Fresh => {
            let _ = writeln!(out, "**{}{} @ {sha}** _(fresh)_", p.path.display(), range);
        }
        PointerStatus::Stale { head_sha } => {
            let _ = writeln!(
                out,
                "**{}{} @ {sha}** _(stale — HEAD is at {head_sha}; \
                 content below is from the locked sha, the working tree may differ)_",
                p.path.display(),
                range,
            );
        }
        PointerStatus::Unresolvable { reason } => {
            let _ = writeln!(
                out,
                "**{}{} @ {sha}** _(unresolvable: {reason})_",
                p.path.display(),
                range,
            );
        }
    }
    if let Some(body) = &r.content {
        let _ = writeln!(out, "```");
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
        let _ = writeln!(out, "```");
    }
}

/// Find every `[[...]]` pointer token in `text` and parse it,
/// dropping unparseable tokens. Used by [`print_role_pointers`] to
/// list what a role's priors reference without doing the full
/// resolution dance — cheaper than `expand_text` and useful for
/// quick audits.
#[must_use]
pub fn extract_pointers(text: &str) -> Vec<Pointer> {
    let mut out = Vec::new();
    for caps in token_re().captures_iter(text) {
        let body = caps.get(1).expect("group 1 matches").as_str();
        if let Some(p) = parse_token(body) {
            out.push(p);
        }
    }
    out
}

/// `cr pointers @<role>` — read the role's priors file, list every
/// pointer it references, and print each one's resolution status
/// (fresh / stale / unresolvable). Mirrors `cr cost`'s shape: print
/// to stdout, return `Result<()>` for clean error surfacing.
pub fn print_role_pointers(project_root: &Path, role: &str) -> Result<()> {
    let coderoom_dir = project_root.join(crate::config::CODEROOM_DIR);
    let priors_path = coderoom_dir
        .join(crate::config::ROLES_DIR)
        .join(format!("{role}.md"));
    let priors = std::fs::read_to_string(&priors_path).map_err(|e| {
        anyhow::anyhow!(
            "could not read priors for @{role} at {}: {e}",
            priors_path.display()
        )
    })?;
    let pointers = extract_pointers(&priors);
    if pointers.is_empty() {
        println!("@{role} has no pointers in its priors file.");
        return Ok(());
    }
    println!("pointers in @{role} priors:");
    // Cache HEAD across the whole list so 10 pointers cost one
    // `git rev-parse HEAD` plus per-pointer `git show`, not N×3.
    let head = resolve_head(project_root);
    let head_ref = head.as_deref();
    for p in &pointers {
        let r = resolve_with_head(p, project_root, head_ref);
        let status_label = match &r.status {
            PointerStatus::Fresh => "fresh".to_owned(),
            PointerStatus::Stale { head_sha } => format!("stale (HEAD at {head_sha})"),
            PointerStatus::Unresolvable { reason } => format!("unresolvable: {reason}"),
        };
        let range = match p.line_range {
            Some((s, e)) if s == e => format!("#L{s}"),
            Some((s, e)) => format!("#L{s}-{e}"),
            None => String::new(),
        };
        let sha = p.locked_sha.as_deref().unwrap_or("HEAD");
        println!(
            "  {} {}{}  @ {sha}  [{status_label}]",
            match &r.status {
                PointerStatus::Fresh => "✓",
                PointerStatus::Stale { .. } => "⚠",
                PointerStatus::Unresolvable { .. } => "✗",
            },
            p.path.display(),
            range,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn init_git_repo(dir: &Path) {
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("git command runs");
        };
        run(&["init", "--quiet", "--initial-branch=main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
    }

    fn commit_file(dir: &Path, rel: &str, content: &str, message: &str) -> String {
        fs::write(dir.join(rel), content).unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("git command runs")
        };
        run(&["add", rel]);
        run(&["commit", "--quiet", "-m", message]);
        let output = run(&["rev-parse", "HEAD"]);
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn parse_token_whole_file_with_sha() {
        let p = parse_token("src/foo.rs@a1b2c3").unwrap();
        assert_eq!(p.path, PathBuf::from("src/foo.rs"));
        assert_eq!(p.line_range, None);
        assert_eq!(p.locked_sha.as_deref(), Some("a1b2c3"));
    }

    #[test]
    fn parse_token_range_with_sha() {
        let p = parse_token("src/foo.rs#L10-42@deadbeef").unwrap();
        assert_eq!(p.line_range, Some((10, 42)));
        assert_eq!(p.locked_sha.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn parse_token_single_line() {
        let p = parse_token("src/foo.rs#L7@aa11").unwrap();
        assert_eq!(p.line_range, Some((7, 7)));
    }

    #[test]
    fn parse_token_no_anchor_at_all_is_rejected() {
        // `[[Cargo.toml]]` with no `#L` and no `@<sha>` could too
        // easily be triggered by prose like `[[TODO]]`. The grammar
        // requires at least one anchor signal to count as a pointer.
        // The user wanting "whole file at HEAD" must spell it
        // `[[Cargo.toml@HEAD]]`.
        assert!(parse_token("Cargo.toml").is_none());
        assert!(parse_token("TODO").is_none());
        assert!(parse_token("README.md").is_none());
    }

    #[test]
    fn parse_token_explicit_head_anchor_is_head_tracking() {
        let p = parse_token("Cargo.toml@HEAD").unwrap();
        assert!(p.locked_sha.is_none());
        assert!(p.line_range.is_none());
        assert_eq!(p.path, PathBuf::from("Cargo.toml"));
    }

    #[test]
    fn parse_token_line_anchor_alone_is_head_tracking() {
        // No `@`, just `#L` — counts as an anchor, points to HEAD.
        let p = parse_token("Cargo.toml#L1").unwrap();
        assert!(p.locked_sha.is_none());
        assert_eq!(p.line_range, Some((1, 1)));
    }

    #[test]
    fn parse_token_rejects_zero_line() {
        assert!(parse_token("src/foo.rs#L0@aa11").is_none());
    }

    #[test]
    fn parse_token_rejects_reversed_range() {
        assert!(parse_token("src/foo.rs#L50-10@aa11").is_none());
    }

    #[test]
    fn parse_token_with_other_non_hex_suffix_is_rejected() {
        // `@HEAD` is special-cased (above) as "explicit head".
        // Anything else after `@` that's not hex falls through to
        // "no anchor at all" and is rejected, to avoid the failure
        // mode where `@main` or `@v1.2` silently becomes part of the
        // path and surprises the user.
        assert!(parse_token("src/foo.rs@main").is_none());
        assert!(parse_token("src/foo.rs@v1.2").is_none());
    }

    #[test]
    fn extract_pointers_drops_anchor_less_tokens() {
        let text = "see [[src/foo.rs#L10@aa11]] and also [[bare-text]] for context";
        let ptrs = extract_pointers(text);
        // `[[bare-text]]` has no `#L` and no `@<sha>` so it's NOT a
        // pointer per the grammar — passes through as plain prose.
        // Only the well-formed one survives. Closes the path-
        // traversal surface where every `[[TODO]]` in prose could
        // trigger a file read.
        assert_eq!(ptrs.len(), 1);
        assert_eq!(ptrs[0].path, PathBuf::from("src/foo.rs"));
    }

    #[test]
    fn read_at_rejects_path_traversal_in_head_tracking_branch() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path());
        commit_file(tmp.path(), "f.rs", "x\n", "init");
        // Build a pointer that escapes the repo via `..`. The SHA-
        // bound branch is gated by git itself; the HEAD-tracking
        // branch needs our own containment check.
        let p = Pointer {
            path: PathBuf::from("../../../../etc/hostname"),
            line_range: None,
            locked_sha: None,
            raw: "[[../../../../etc/hostname@HEAD]]".into(),
        };
        let r = resolve(&p, tmp.path());
        assert!(
            matches!(r.status, PointerStatus::Unresolvable { .. }),
            "expected Unresolvable, got {:?}",
            r.status
        );
        assert!(r.content.is_none());
    }

    #[test]
    fn read_at_rejects_absolute_path_in_head_tracking_branch() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path());
        commit_file(tmp.path(), "f.rs", "x\n", "init");
        let p = Pointer {
            path: PathBuf::from("/etc/hostname"),
            line_range: None,
            locked_sha: None,
            raw: "[[/etc/hostname@HEAD]]".into(),
        };
        let r = resolve(&p, tmp.path());
        assert!(matches!(r.status, PointerStatus::Unresolvable { .. }));
    }

    #[test]
    fn resolve_fresh_pointer_at_head() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path());
        let sha = commit_file(tmp.path(), "f.rs", "line1\nline2\nline3\n", "init");
        let p = parse_token(&format!("f.rs#L2@{sha}")).unwrap();
        let r = resolve(&p, tmp.path());
        assert!(matches!(r.status, PointerStatus::Fresh));
        assert_eq!(r.content.as_deref(), Some("line2"));
    }

    #[test]
    fn resolve_stale_pointer_keeps_content_from_locked_sha() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path());
        let sha_old = commit_file(tmp.path(), "f.rs", "original\n", "init");
        let _sha_new = commit_file(tmp.path(), "f.rs", "rewritten\n", "rewrite");
        let p = parse_token(&format!("f.rs@{sha_old}")).unwrap();
        let r = resolve(&p, tmp.path());
        assert!(matches!(r.status, PointerStatus::Stale { .. }));
        // Content reflects what the priors author *saw*, not HEAD —
        // staying faithful to the recorded reference. The status line
        // tells the reader HEAD has moved.
        assert_eq!(r.content.as_deref(), Some("original\n"));
    }

    #[test]
    fn resolve_unknown_sha_is_unresolvable() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path());
        commit_file(tmp.path(), "f.rs", "data\n", "init");
        let p = parse_token("f.rs@000000").unwrap();
        let r = resolve(&p, tmp.path());
        assert!(matches!(r.status, PointerStatus::Unresolvable { .. }));
        assert!(r.content.is_none());
    }

    #[test]
    fn resolve_head_tracking_reads_working_tree() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path());
        commit_file(tmp.path(), "f.rs", "committed\n", "init");
        // Edit the working tree past the commit; HEAD-tracking
        // pointer should pick up the uncommitted text.
        fs::write(tmp.path().join("f.rs"), "uncommitted\n").unwrap();
        // `@HEAD` is the explicit anchor for HEAD-tracking — the
        // wholly-unanchored `[[f.rs]]` form is rejected at parse so
        // prose tokens can't trigger filesystem reads.
        let p = parse_token("f.rs@HEAD").unwrap();
        let r = resolve(&p, tmp.path());
        assert!(matches!(r.status, PointerStatus::Fresh));
        assert_eq!(r.content.as_deref(), Some("uncommitted\n"));
    }

    #[test]
    fn expand_text_substitutes_pointer_with_markdown_section() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path());
        let sha = commit_file(tmp.path(), "f.rs", "a\nb\nc\n", "init");
        let priors = format!("Watch the cc adapter:\n[[f.rs#L2@{sha}]]\nThat's the line.\n");
        let out = expand_text(&priors, tmp.path());
        assert!(out.contains("**f.rs#L2"));
        assert!(out.contains("(fresh)"));
        assert!(out.contains("```\nb\n```"));
        // Surrounding prose is preserved.
        assert!(out.contains("Watch the cc adapter:"));
        assert!(out.contains("That's the line."));
    }

    #[test]
    fn expand_text_marks_stale_pointer() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path());
        let old = commit_file(tmp.path(), "f.rs", "original\n", "init");
        commit_file(tmp.path(), "f.rs", "rewritten\n", "rewrite");
        let priors = format!("[[f.rs@{old}]]");
        let out = expand_text(&priors, tmp.path());
        assert!(out.contains("(stale"));
        assert!(out.contains("HEAD is at"));
        assert!(out.contains("```\noriginal\n```"));
    }

    #[test]
    fn expand_text_passes_unparseable_tokens_through() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path());
        // `[[  ]]` contains whitespace inside, regex won't match.
        // `[[]]` has empty body, also won't match the non-empty
        // pattern. We test a token the regex matches but parse
        // rejects: `[[src/foo.rs#L-1@aa11]]` (negative line).
        let priors = "skip [[src/foo.rs#L-1@aa11]] please";
        let out = expand_text(priors, tmp.path());
        assert_eq!(out, priors);
    }
}
