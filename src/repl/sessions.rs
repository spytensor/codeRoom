//! Per-role session-id persistence for engine resume.
//!
//! Each engine adapter that supports session resume (cc's
//! `--resume <id>`; codex / gemini equivalents in follow-up work)
//! emits the session id it created on its `RoleStarted` event.
//! `cr start` writes that id to `.coderoom/sessions/<role>.id`; the
//! next invocation reads it back and threads it into `RoleConfig` so
//! the engine resumes the prior conversation instead of starting
//! fresh. Per amendment A-006, resume is the default behaviour — the
//! user types `cr start --fresh` to opt out.
//!
//! These files are intentionally outside the project's git tree.
//! Session ids are pointers into the engine's *local* storage (e.g.
//! `~/.claude/projects/<hash>/sessions/`) so they don't make sense
//! across machines.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::CODEROOM_DIR;

/// Directory under a project's `.coderoom/` holding per-role session
/// ids. Created on demand by [`write_session_id`]. The default
/// `.coderoom/.gitignore` excludes `sessions/`; session ids are
/// pointers into the engine's local storage and should not be
/// committed.
///
/// We live in a `sessions/ids/` subdir specifically so [`clear_all`]
/// can wipe every per-role id with a single `remove_dir_all` without
/// trampling the init wizard's `sessions/role-suggestions-dismissed`
/// marker (`src/init.rs`), which shares the parent `sessions/`
/// directory for unrelated reasons.
pub(super) const SESSIONS_DIR: &str = "sessions";
pub(super) const SESSION_IDS_SUBDIR: &str = "ids";

/// Path of the session-id file for `role` under `project_root`.
pub(super) fn session_id_path(project_root: &Path, role: &str) -> PathBuf {
    project_root
        .join(CODEROOM_DIR)
        .join(SESSIONS_DIR)
        .join(SESSION_IDS_SUBDIR)
        .join(format!("{role}.id"))
}

/// Read the persisted session id for `role`. Returns `Ok(None)` when
/// the file does not exist (no previous session), `Ok(Some(id))` when
/// a non-empty id is found, and `Err` only on unexpected I/O failures
/// — a missing or empty file is the normal first-run path.
pub(super) fn read_session_id(project_root: &Path, role: &str) -> io::Result<Option<String>> {
    let path = session_id_path(project_root, role);
    match fs::read_to_string(&path) {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_owned()))
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Write `session_id` as the latest session for `role`. Overwrites
/// any previous value. Creates the parent `.coderoom/sessions/`
/// directory on demand.
pub(super) fn write_session_id(
    project_root: &Path,
    role: &str,
    session_id: &str,
) -> io::Result<()> {
    let path = session_id_path(project_root, role);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, session_id.trim().as_bytes())
}

/// Remove the persisted session id for `role`. No-op when the file
/// does not exist. Used by `/refresh @role` so a fresh-priors reload
/// also resets the role's conversation, and by `cr start --fresh`
/// which clears every role at once.
#[allow(dead_code, reason = "wired by PR-7 (cr start --fresh + /refresh)")]
pub(super) fn clear_session_id(project_root: &Path, role: &str) -> io::Result<()> {
    let path = session_id_path(project_root, role);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Remove every persisted session id under `project_root`. Used by
/// `cr start --fresh`. Only touches the `sessions/ids/` subdir so
/// the init wizard's `sessions/role-suggestions-dismissed` marker
/// and any other future sibling under `sessions/` are preserved.
/// Returns `Ok(())` when the `sessions/ids/` directory does not
/// exist at all.
#[allow(dead_code, reason = "wired by PR-7 (cr start --fresh)")]
pub(super) fn clear_all(project_root: &Path) -> io::Result<()> {
    let dir = project_root
        .join(CODEROOM_DIR)
        .join(SESSIONS_DIR)
        .join(SESSION_IDS_SUBDIR);
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(CODEROOM_DIR)).unwrap();
        dir
    }

    #[test]
    fn read_returns_none_when_file_missing() {
        let project = fixture();
        assert_eq!(read_session_id(project.path(), "host").unwrap(), None);
    }

    #[test]
    fn write_then_read_roundtrips_the_id() {
        let project = fixture();
        write_session_id(project.path(), "host", "abc-123").unwrap();
        assert_eq!(
            read_session_id(project.path(), "host").unwrap().as_deref(),
            Some("abc-123")
        );
    }

    #[test]
    fn write_trims_whitespace_so_round_trip_is_stable() {
        let project = fixture();
        write_session_id(project.path(), "host", "  abc-123\n").unwrap();
        assert_eq!(
            read_session_id(project.path(), "host").unwrap().as_deref(),
            Some("abc-123")
        );
    }

    #[test]
    fn write_overwrites_previous_value() {
        let project = fixture();
        write_session_id(project.path(), "host", "old").unwrap();
        write_session_id(project.path(), "host", "new").unwrap();
        assert_eq!(
            read_session_id(project.path(), "host").unwrap().as_deref(),
            Some("new")
        );
    }

    #[test]
    fn read_treats_empty_file_as_no_session() {
        let project = fixture();
        write_session_id(project.path(), "host", "   ").unwrap();
        // An all-whitespace file is the same as a missing file —
        // resume should NOT be attempted with an empty id.
        assert_eq!(read_session_id(project.path(), "host").unwrap(), None);
    }

    #[test]
    fn clear_role_session_removes_the_id() {
        let project = fixture();
        write_session_id(project.path(), "host", "abc-123").unwrap();
        clear_session_id(project.path(), "host").unwrap();
        assert_eq!(read_session_id(project.path(), "host").unwrap(), None);
        // Idempotent on second clear.
        clear_session_id(project.path(), "host").unwrap();
    }

    #[test]
    fn clear_all_removes_every_role_session() {
        let project = fixture();
        write_session_id(project.path(), "host", "a").unwrap();
        write_session_id(project.path(), "backend", "b").unwrap();
        write_session_id(project.path(), "security", "c").unwrap();

        clear_all(project.path()).unwrap();
        // The ids subdir is gone, but `sessions/` parent stays.
        assert!(!project
            .path()
            .join(CODEROOM_DIR)
            .join(SESSIONS_DIR)
            .join(SESSION_IDS_SUBDIR)
            .exists());
        // Subsequent reads are clean.
        for role in ["host", "backend", "security"] {
            assert_eq!(read_session_id(project.path(), role).unwrap(), None);
        }
    }

    #[test]
    fn clear_all_preserves_init_wizard_marker() {
        // The init wizard's role-suggestions-dismissed marker lives
        // at .coderoom/sessions/role-suggestions-dismissed (sibling
        // of our ids/ subdir). `cr start --fresh` must not delete
        // it — otherwise the role-expansion picker would re-fire
        // on every fresh start, which the user already dismissed.
        let project = fixture();
        let sessions_dir = project.path().join(CODEROOM_DIR).join(SESSIONS_DIR);
        fs::create_dir_all(&sessions_dir).unwrap();
        let marker = sessions_dir.join("role-suggestions-dismissed");
        fs::write(&marker, "1").unwrap();

        write_session_id(project.path(), "host", "abc").unwrap();
        clear_all(project.path()).unwrap();

        assert!(marker.exists(), "init wizard marker must survive clear_all");
        assert_eq!(read_session_id(project.path(), "host").unwrap(), None);
    }

    #[test]
    fn clear_all_is_idempotent_when_directory_missing() {
        let project = fixture();
        // Never wrote anything; clear_all should still succeed.
        clear_all(project.path()).unwrap();
    }
}
