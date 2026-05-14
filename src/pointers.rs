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
///
/// Round-trips through markdown via `Display` and `FromStr`:
/// ```ignore
/// let p = Pointer::from_str("src/foo.rs#L42-67@a1b2c3").unwrap();
/// assert_eq!(p.to_string(), "src/foo.rs#L42-67@a1b2c3");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pointer {
    /// Repo-relative path.
    pub path: PathBuf,
    /// Inclusive 1-based line range. `None` means whole-file.
    pub line_range: Option<(u32, u32)>,
    /// Git object id the pointer is locked to. `None` means "HEAD,
    /// no staleness check" — the escape hatch.
    pub locked_sha: Option<String>,
}

impl std::fmt::Display for Pointer {
    /// Canonical token form, **without** the surrounding `[[ ]]`.
    /// Wrap with brackets at render time when emitting into priors
    /// markdown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path.display())?;
        if let Some((start, end)) = self.line_range {
            if start == end {
                write!(f, "#L{start}")?;
            } else {
                write!(f, "#L{start}-{end}")?;
            }
        }
        if let Some(sha) = &self.locked_sha {
            write!(f, "@{sha}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for Pointer {
    type Err = ();
    /// Parse a token *body* (without the surrounding `[[ ]]`). Use
    /// [`parse_token`] for the same operation with an explicit
    /// `Option` return — `FromStr` exists so `Pointer` can ride
    /// through `serde` / `clap` / config files without bespoke code.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_token(s).ok_or(())
    }
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
    /// Pointer could not be resolved at all. The typed reason lets
    /// downstream consumers (Contracts, Inbox, future evolution
    /// loop) discriminate on cause without string-parsing.
    Unresolvable(UnresolvableReason),
}

/// Why a pointer couldn't be resolved. Each variant maps to a distinct
/// remediation path on the user side, so downstream surfaces (cr
/// pointers output, contract validation, evolution-loop checks) can
/// route on the typed value instead of reading prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnresolvableReason {
    /// `git rev-parse HEAD` failed — most likely the project isn't
    /// inside a git working tree. `repo_root` is captured so error
    /// messages can point the user at the directory checked.
    NotAGitRepo {
        /// Path that was checked (the `repo_root` argument).
        repo_root: PathBuf,
    },
    /// The locked SHA is not known to this clone. Usually a force-
    /// push discarded it, or the priors author referenced a SHA from
    /// a different branch that was never fetched.
    ShaNotFound {
        /// The hex object id from the pointer's `@<sha>` suffix.
        sha: String,
    },
    /// HEAD-tracking path canonicalised to a location outside
    /// `repo_root`. Security gate against `[[../../etc/passwd@HEAD]]`
    /// style tokens. Captures both paths so the error message can
    /// explain *why* the read was refused.
    PathEscapesRepo {
        /// Canonical absolute path the pointer resolved to.
        attempted: PathBuf,
        /// Canonical repo root the path had to stay inside.
        repo_root: PathBuf,
    },
    /// Path doesn't exist at the locked SHA — e.g. the file was
    /// renamed or deleted in a later commit, and the priors author
    /// pinned to a version that no longer matches the working tree.
    PathNotFoundAtSha {
        /// Repo-relative path that wasn't found.
        path: PathBuf,
        /// The locked SHA the lookup was attempted against.
        sha: String,
    },
    /// I/O error other than path-not-found (permission, disk, etc.).
    /// Captured as a string because the underlying [`std::io::Error`]
    /// kinds aren't useful enough to enumerate.
    Io(String),
    /// `git` invocation failed for a reason we couldn't classify into
    /// one of the typed variants above (network during fetch, broken
    /// `.git/`, etc.).
    Git(String),
}

impl std::fmt::Display for UnresolvableReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAGitRepo { repo_root } => write!(
                f,
                "not a git repo at {} — run from a git working tree, or pass --project",
                repo_root.display(),
            ),
            Self::ShaNotFound { sha } => write!(
                f,
                "locked sha `{sha}` is not in this repo \
                 (use `git log --oneline -- <path>` to find a valid sha, \
                 or `git fetch` if it's on a remote branch)",
            ),
            Self::PathEscapesRepo {
                attempted,
                repo_root,
            } => write!(
                f,
                "pointer path {} leaves the project \
                 (pointers must reference files inside {} — \
                 this is a security gate against `@HEAD` reads of files \
                 outside the repo)",
                attempted.display(),
                repo_root.display(),
            ),
            Self::PathNotFoundAtSha { path, sha } => write!(
                f,
                "path {} does not exist at locked sha `{sha}` \
                 (the file may have been renamed or deleted since — \
                 update the pointer's path or move it to `@HEAD`)",
                path.display(),
            ),
            Self::Io(msg) => write!(f, "i/o error: {msg}"),
            Self::Git(msg) => write!(f, "git error: {msg}"),
        }
    }
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
        PointerStatus::Unresolvable(_) => (None, status.clone()),
        PointerStatus::Fresh | PointerStatus::Stale { .. } => match read_at(pointer, repo_root) {
            Ok(text) => (Some(text), status),
            Err(reason) => (None, PointerStatus::Unresolvable(reason)),
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
        return PointerStatus::Unresolvable(UnresolvableReason::NotAGitRepo {
            repo_root: repo_root.to_path_buf(),
        });
    };
    // Resolve the locked sha to its full form. If git can't, the
    // priors reference a commit that doesn't exist in this clone
    // (e.g. a force-push discarded it). Mark unresolvable explicitly
    // — different from "stale" because user action is needed.
    let locked_full = match run_git(repo_root, &["rev-parse", locked_sha]) {
        Ok(s) => s.trim().to_owned(),
        Err(_) => {
            return PointerStatus::Unresolvable(UnresolvableReason::ShaNotFound {
                sha: locked_sha.to_owned(),
            });
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

fn read_at(pointer: &Pointer, repo_root: &Path) -> Result<String, UnresolvableReason> {
    let raw = if let Some(sha) = pointer.locked_sha.as_deref() {
        // `git show <sha>:<path>` is safe by construction — git
        // explicitly rejects `../` and absolute paths with "path
        // outside repository". Argv (not shell) so no metacharacter
        // escape concerns either.
        run_git(
            repo_root,
            &["show", &format!("{sha}:{}", path_for_git(&pointer.path))],
        )
        .map_err(|e| {
            // Git's "exists in <tree>, exists on disk" vs "does not
            // exist in <tree>" diagnostics — we route the latter to
            // the typed PathNotFoundAtSha so downstream UX can offer
            // "use @HEAD or update the path."
            let msg = e.to_string();
            if msg.contains("does not exist") || msg.contains("exists on disk") {
                UnresolvableReason::PathNotFoundAtSha {
                    path: pointer.path.clone(),
                    sha: sha.to_owned(),
                }
            } else {
                UnresolvableReason::Git(msg)
            }
        })?
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
            .map_err(|e| UnresolvableReason::Io(format!("canonicalising repo root: {e}")))?;
        let canonical_full = full.canonicalize().map_err(|e| {
            UnresolvableReason::Io(format!("canonicalising {}: {e}", full.display()))
        })?;
        if !canonical_full.starts_with(&canonical_root) {
            return Err(UnresolvableReason::PathEscapesRepo {
                attempted: canonical_full,
                repo_root: canonical_root,
            });
        }
        std::fs::read_to_string(&canonical_full).map_err(|e| {
            UnresolvableReason::Io(format!("reading {}: {e}", canonical_full.display()))
        })?
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
    // `path[#L…][@<short-sha>]` — the `Display` impl on Pointer
    // emits the canonical form, but we want the locked SHA shown
    // in short form here for terminal readability. Build a shadow
    // pointer with the truncated SHA and Display *that*.
    let header_pointer = Pointer {
        path: p.path.clone(),
        line_range: p.line_range,
        locked_sha: p
            .locked_sha
            .as_ref()
            .map(|s| s.chars().take(8).collect::<String>()),
    };
    let _ = writeln!(out);
    match &r.status {
        PointerStatus::Fresh => {
            let _ = writeln!(out, "**[[{header_pointer}]]** _(fresh)_");
        }
        PointerStatus::Stale { head_sha } => {
            let _ = writeln!(
                out,
                "**[[{header_pointer}]]** _(⚠ stale — HEAD is at {head_sha}; \
                 content below is from the locked sha, the working tree may differ)_",
            );
        }
        PointerStatus::Unresolvable(reason) => {
            let _ = writeln!(out, "**[[{header_pointer}]]** _(unresolvable: {reason})_");
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

/// Resolve every pointer in `text` against `repo_root` with a single
/// shared HEAD lookup. Caller decides how to render — used by the
/// `cr pointers` subcommand and could be reused by a future TUI
/// inspection surface. Pure data in / data out: this module no
/// longer reaches into `crate::config` or `println!`s.
#[must_use]
pub fn resolve_all(text: &str, repo_root: &Path) -> Vec<Resolved> {
    let head = resolve_head(repo_root);
    let head_ref = head.as_deref();
    extract_pointers(text)
        .into_iter()
        .map(|p| resolve_with_head(&p, repo_root, head_ref))
        .collect()
}

/// Short status word for `cr pointers` table output. Pure function
/// so the CLI rendering path can stay in `main.rs` without re-
/// implementing the status taxonomy.
#[must_use]
pub fn status_word(status: &PointerStatus) -> &'static str {
    match status {
        PointerStatus::Fresh => "fresh",
        PointerStatus::Stale { .. } => "stale",
        PointerStatus::Unresolvable(_) => "unresolvable",
    }
}

/// Status glyph for terminal rendering. Returns plain ASCII when the
/// caller is going to wrap with colour; for piped/non-TTY output, the
/// caller can substitute `[ok]/[stale]/[bad]` brackets.
#[must_use]
pub fn status_glyph(status: &PointerStatus) -> &'static str {
    match status {
        PointerStatus::Fresh => "✓",
        PointerStatus::Stale { .. } => "⚠",
        PointerStatus::Unresolvable(_) => "✗",
    }
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
    fn pointer_round_trips_through_display_and_from_str() {
        use std::str::FromStr;
        // Canonical forms — parse and Display agree exactly.
        for body in [
            "src/foo.rs#L42-67@a1b2c3",
            "src/foo.rs#L7@deadbeef",
            "src/foo.rs@aabbccdd",
            "src/foo.rs#L1-2",
        ] {
            let parsed =
                Pointer::from_str(body).unwrap_or_else(|()| panic!("should parse: {body}"));
            assert_eq!(parsed.to_string(), body, "round-trip failed for {body}");
        }
    }

    #[test]
    fn pointer_head_anchor_canonicalises_away_at_serialize() {
        use std::str::FromStr;
        // `@HEAD` and the equivalent line-anchored form both parse,
        // but the canonical `Display` drops `@HEAD` since locked_sha
        // is None — semantically identical, no information lost.
        let with = Pointer::from_str("src/foo.rs#L1@HEAD").unwrap();
        let without = Pointer::from_str("src/foo.rs#L1").unwrap();
        assert_eq!(with, without);
        assert_eq!(with.to_string(), "src/foo.rs#L1");
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

    /// Path two `TempDir`s end up being siblings (e.g.
    /// `/tmp/.tmpAAA` and `/tmp/.tmpBBB`), and a relative path from
    /// one to the other walks up the shared parent. Compute that
    /// relative path so the path-traversal tests don't rely on
    /// platform-specific files like `/etc/hostname` (which exists on
    /// Linux but not on macOS — caught by CI).
    fn relative_escape_path(from: &Path, to: &Path) -> PathBuf {
        // Both TempDir paths share `std::env::temp_dir()` as their
        // parent on Unix and Windows; we don't need a general-purpose
        // path-diff — just `..` plus the target's last component.
        let from = from.canonicalize().unwrap();
        let to = to.canonicalize().unwrap();
        let common = from
            .parent()
            .filter(|p| to.starts_with(p))
            .unwrap_or_else(|| panic!("temp dirs don't share a parent: {from:?} vs {to:?}"));
        let mut rel = PathBuf::new();
        let from_remainder = from.strip_prefix(common).unwrap();
        for _ in from_remainder.components() {
            rel.push("..");
        }
        rel.push(to.strip_prefix(common).unwrap());
        rel
    }

    #[test]
    fn read_at_rejects_path_traversal_in_head_tracking_branch() {
        // Create a file in a *separate* tmpdir so the "escape target"
        // is guaranteed to exist on both Linux and macOS. The earlier
        // version pointed at `/etc/hostname`, which doesn't exist on
        // macOS — canonicalize failed before the containment check
        // could fire, so the test asserted the wrong branch.
        let outside = TempDir::new().unwrap();
        let outside_secret = outside.path().join("secret.txt");
        fs::write(&outside_secret, "exfiltrate me\n").unwrap();

        let repo = TempDir::new().unwrap();
        init_git_repo(repo.path());
        commit_file(repo.path(), "f.rs", "x\n", "init");

        // Relative path from inside the repo to the outside secret —
        // walks `..` past the repo root then back down. The pointer
        // path is relative on purpose: it's the most common shape a
        // priors file would carry.
        let escape = relative_escape_path(repo.path(), &outside_secret);
        let p = Pointer {
            path: escape,
            line_range: None,
            locked_sha: None,
        };
        let r = resolve(&p, repo.path());
        // The typed `PathEscapesRepo` reason flows out, not a generic
        // string — downstream UX can route on the variant.
        assert!(
            matches!(
                r.status,
                PointerStatus::Unresolvable(UnresolvableReason::PathEscapesRepo { .. })
            ),
            "expected PathEscapesRepo, got {:?}",
            r.status
        );
        assert!(r.content.is_none());
    }

    #[test]
    fn read_at_rejects_absolute_path_in_head_tracking_branch() {
        // Same fix as the relative-path test: use a tmpfile we control
        // so the target exists on every platform CI runs on.
        let outside = TempDir::new().unwrap();
        let outside_secret = outside.path().join("secret.txt");
        fs::write(&outside_secret, "exfiltrate me\n").unwrap();

        let repo = TempDir::new().unwrap();
        init_git_repo(repo.path());
        commit_file(repo.path(), "f.rs", "x\n", "init");

        let p = Pointer {
            path: outside_secret.canonicalize().unwrap(),
            line_range: None,
            locked_sha: None,
        };
        let r = resolve(&p, repo.path());
        assert!(matches!(
            r.status,
            PointerStatus::Unresolvable(UnresolvableReason::PathEscapesRepo { .. })
        ));
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
        // Typed reason: ShaNotFound, not a generic Unresolvable string.
        assert!(matches!(
            r.status,
            PointerStatus::Unresolvable(UnresolvableReason::ShaNotFound { ref sha }) if sha == "000000"
        ));
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
        // Resolved header carries the canonical token form (with the
        // SHA truncated to 8 chars for terminal width).
        assert!(out.contains("**[[f.rs#L2@"));
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
        assert!(out.contains("⚠ stale"));
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
