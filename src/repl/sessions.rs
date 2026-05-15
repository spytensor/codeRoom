//! Per-role session-id persistence for engine resume.
//!
//! Engine adapters that support session resume emit the session id
//! they created on `RoleStarted` or `RoleSessionUpdated`. `cr start`
//! writes that id to `.coderoom/sessions/ids/<role>.id`; the next
//! invocation reads it back and threads it into `RoleConfig` so the
//! engine resumes the prior conversation instead of starting fresh.
//! Per amendment A-006, resume is the default behaviour — the user
//! types `cr start --fresh` to opt out.
//!
//! These files are intentionally outside the project's git tree.
//! Session ids are pointers into the engine's *local* storage (e.g.
//! `~/.claude/projects/<hash>/sessions/`) so they don't make sense
//! across machines.

use std::fmt::Write as FmtWrite;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result as AnyResult;
use crossterm::style::Stylize;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::config::CODEROOM_DIR;
use crate::init::{read_key, WizardKey, WizardTerminal};
use crate::output;

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
pub(super) const ROOM_SESSIONS_SUBDIR: &str = "rooms";
const CURRENT_ROOM_FILE: &str = "current-room.id";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct RoomSession {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub role_sessions: BTreeMap<String, String>,
}

/// Path of the session-id file for `role` under `project_root`.
pub(super) fn session_id_path(project_root: &Path, role: &str) -> PathBuf {
    project_root
        .join(CODEROOM_DIR)
        .join(SESSIONS_DIR)
        .join(SESSION_IDS_SUBDIR)
        .join(format!("{role}.id"))
}

fn room_sessions_dir(project_root: &Path) -> PathBuf {
    project_root
        .join(CODEROOM_DIR)
        .join(SESSIONS_DIR)
        .join(ROOM_SESSIONS_SUBDIR)
}

fn room_session_path(project_root: &Path, id: &str) -> PathBuf {
    room_sessions_dir(project_root).join(format!("{id}.json"))
}

fn current_room_path(project_root: &Path) -> PathBuf {
    project_root
        .join(CODEROOM_DIR)
        .join(SESSIONS_DIR)
        .join(CURRENT_ROOM_FILE)
}

fn now_stamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn new_room_id() -> String {
    format!("room-{}", chrono::Utc::now().timestamp_millis())
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

fn read_all_session_ids(project_root: &Path) -> io::Result<BTreeMap<String, String>> {
    let dir = project_root
        .join(CODEROOM_DIR)
        .join(SESSIONS_DIR)
        .join(SESSION_IDS_SUBDIR);
    let mut ids = BTreeMap::new();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(ids),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("id") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let text = fs::read_to_string(&path)?;
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            ids.insert(stem.to_owned(), trimmed.to_owned());
        }
    }
    Ok(ids)
}

fn write_room_session(project_root: &Path, session: &RoomSession) -> io::Result<()> {
    let path = room_session_path(project_root, &session.id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(session).map_err(io::Error::other)?;
    fs::write(path, text)
}

pub(super) fn read_room_session(project_root: &Path, id: &str) -> io::Result<RoomSession> {
    let path = room_session_path(project_root, id);
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(io::Error::other)
}

fn write_current_room_id(project_root: &Path, id: &str) -> io::Result<()> {
    let path = current_room_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, id.trim().as_bytes())
}

pub(super) fn read_current_room_id(project_root: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(current_room_path(project_root)) {
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

fn create_room_session_with_roles(
    project_root: &Path,
    role_sessions: BTreeMap<String, String>,
) -> io::Result<RoomSession> {
    let mut id = new_room_id();
    let mut suffix = 2usize;
    while room_session_path(project_root, &id).exists() {
        id = format!("{}-{suffix}", new_room_id());
        suffix += 1;
    }
    let now = now_stamp();
    let session = RoomSession {
        id: id.clone(),
        created_at: now.clone(),
        updated_at: now,
        role_sessions,
    };
    write_room_session(project_root, &session)?;
    write_current_room_id(project_root, &id)?;
    Ok(session)
}

pub(super) fn ensure_current_room_session(project_root: &Path) -> io::Result<RoomSession> {
    if let Some(current) = read_current_room_id(project_root)? {
        if let Ok(session) = read_room_session(project_root, &current) {
            return Ok(session);
        }
    }
    create_room_session_with_roles(project_root, read_all_session_ids(project_root)?)
}

pub(super) fn start_new_room_session(project_root: &Path) -> io::Result<RoomSession> {
    create_room_session_with_roles(project_root, BTreeMap::new())
}

pub(super) fn record_current_room_role_session(
    project_root: &Path,
    role: &str,
    session_id: &str,
) -> io::Result<()> {
    let mut session = ensure_current_room_session(project_root)?;
    session
        .role_sessions
        .insert(role.to_owned(), session_id.trim().to_owned());
    session.updated_at = now_stamp();
    write_room_session(project_root, &session)
}

pub(super) fn remove_current_room_role(project_root: &Path, role: &str) -> io::Result<()> {
    let mut session = ensure_current_room_session(project_root)?;
    session.role_sessions.remove(role);
    session.updated_at = now_stamp();
    write_room_session(project_root, &session)
}

pub(super) fn list_room_sessions(project_root: &Path) -> io::Result<Vec<RoomSession>> {
    let dir = room_sessions_dir(project_root);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let text = fs::read_to_string(&path)?;
        let session = serde_json::from_str::<RoomSession>(&text).map_err(io::Error::other)?;
        sessions.push(session);
    }
    sessions.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(sessions)
}

pub(super) fn resolve_room_session(
    project_root: &Path,
    selector: &str,
) -> io::Result<Option<RoomSession>> {
    let sessions = list_room_sessions(project_root)?;
    let trimmed = selector.trim();
    if trimmed.is_empty() || trimmed == "latest" {
        return Ok(sessions.into_iter().next());
    }
    if let Ok(index) = trimmed.parse::<usize>() {
        return Ok(sessions.get(index.saturating_sub(1)).cloned());
    }
    if let Some(exact) = sessions.iter().find(|session| session.id == trimmed) {
        return Ok(Some(exact.clone()));
    }
    let mut matches = sessions
        .into_iter()
        .filter(|session| session.id.starts_with(trimmed))
        .collect::<Vec<_>>();
    if matches.len() == 1 {
        Ok(matches.pop())
    } else {
        Ok(None)
    }
}

/// Full-screen arrow-key picker over saved room sessions. Returns
/// `Ok(Some(session))` when the user picks one, `Ok(None)` when they
/// cancel (Esc / Ctrl-C) or no sessions exist. Drives the bare
/// `/resume` flow in the REPL; callers with an explicit selector
/// still go through [`resolve_room_session`] instead.
pub(super) fn pick_room_session(project_root: &Path) -> AnyResult<Option<RoomSession>> {
    let sessions = list_room_sessions(project_root)?;
    if sessions.is_empty() {
        return Ok(None);
    }
    let current = read_current_room_id(project_root)
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut cursor = sessions
        .iter()
        .position(|session| session.id == current)
        .unwrap_or(0);
    let mut terminal = WizardTerminal::enter()?;
    loop {
        terminal.render(&render_session_picker(&sessions, &current, cursor))?;
        match read_key()? {
            WizardKey::Abort | WizardKey::Back => return Ok(None),
            WizardKey::Up => cursor = cursor.saturating_sub(1),
            WizardKey::Down => {
                if cursor + 1 < sessions.len() {
                    cursor += 1;
                }
            }
            WizardKey::Enter => return Ok(Some(sessions[cursor].clone())),
            WizardKey::Left | WizardKey::Right | WizardKey::Toggle => {}
        }
    }
}

/// Renderable picker body. Pure function for snapshot testing; the
/// real picker re-paints this on every keypress.
pub(super) fn render_session_picker(
    sessions: &[RoomSession],
    current: &str,
    cursor: usize,
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        "Resume which CodeRoom session?".with(output::TEXT).bold()
    );
    let _ = writeln!(
        out,
        "{}",
        "  ↑↓ moves · enter selects · esc cancels".with(output::DIM)
    );
    let _ = writeln!(out);
    for (index, session) in sessions.iter().enumerate() {
        let cursor_glyph = if index == cursor { "> " } else { "  " };
        let marker = if session.id == current {
            "(current)"
        } else {
            ""
        };
        let role_count = session.role_sessions.len();
        let role_word = if role_count == 1 {
            "session"
        } else {
            "sessions"
        };
        let plain = format!(
            "{cursor_glyph}{}  updated {}  {role_count} role {role_word}  {marker}",
            session.id, session.updated_at,
        );
        if index == cursor {
            let _ = writeln!(out, "{}", plain.with(output::EM));
        } else {
            let _ = writeln!(out, "{plain}");
        }
    }
    out
}

pub(super) fn activate_room_session(project_root: &Path, session: &RoomSession) -> io::Result<()> {
    clear_all(project_root)?;
    for (role, session_id) in &session.role_sessions {
        if !session_id.trim().is_empty() {
            write_session_id(project_root, role, session_id)?;
        }
    }
    write_current_room_id(project_root, &session.id)
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
    fn ensure_current_room_adopts_existing_role_ids() {
        let project = fixture();
        write_session_id(project.path(), "host", "h1").unwrap();
        write_session_id(project.path(), "qa", "q1").unwrap();

        let room = ensure_current_room_session(project.path()).unwrap();
        assert_eq!(
            room.role_sessions.get("host").map(String::as_str),
            Some("h1")
        );
        assert_eq!(room.role_sessions.get("qa").map(String::as_str), Some("q1"));
        assert_eq!(
            read_current_room_id(project.path()).unwrap().as_deref(),
            Some(room.id.as_str())
        );
    }

    #[test]
    fn record_current_room_role_session_updates_room_snapshot() {
        let project = fixture();
        let room = start_new_room_session(project.path()).unwrap();

        record_current_room_role_session(project.path(), "qa", "thread-1").unwrap();

        let updated = read_room_session(project.path(), &room.id).unwrap();
        assert_eq!(
            updated.role_sessions.get("qa").map(String::as_str),
            Some("thread-1")
        );
    }

    #[test]
    fn start_new_room_session_creates_empty_current_room() {
        let project = fixture();
        write_session_id(project.path(), "host", "old-session").unwrap();

        let room = start_new_room_session(project.path()).unwrap();

        assert!(room.role_sessions.is_empty());
        assert_eq!(
            read_current_room_id(project.path()).unwrap().as_deref(),
            Some(room.id.as_str())
        );
        assert_eq!(
            read_room_session(project.path(), &room.id)
                .unwrap()
                .role_sessions,
            BTreeMap::new()
        );
        assert_eq!(
            read_session_id(project.path(), "host").unwrap().as_deref(),
            Some("old-session"),
            "callers clear ids separately so the room snapshot stays explicit",
        );
    }

    #[test]
    fn picker_renders_cursor_glyph_on_active_row() {
        let sessions = vec![
            RoomSession {
                id: "room-aaa".into(),
                created_at: "2026-05-13T16:00:00Z".into(),
                updated_at: "2026-05-13T17:00:00Z".into(),
                role_sessions: BTreeMap::from([("host".into(), "h1".into())]),
            },
            RoomSession {
                id: "room-bbb".into(),
                created_at: "2026-05-13T15:00:00Z".into(),
                updated_at: "2026-05-13T16:00:00Z".into(),
                role_sessions: BTreeMap::from([
                    ("host".into(), "h2".into()),
                    ("qa".into(), "q2".into()),
                ]),
            },
        ];

        let body = render_session_picker(&sessions, "room-bbb", 1);
        // Cursor row is index 1 → only that row carries the "> " glyph.
        let first_row = body.lines().nth(3).expect("first session row");
        let second_row = body.lines().nth(4).expect("second session row");
        assert!(
            first_row.contains("  room-aaa"),
            "non-cursor row should start with indent, got {first_row:?}"
        );
        assert!(
            second_row.contains("> room-bbb"),
            "cursor row should carry the > glyph, got {second_row:?}"
        );
        // Current session marker travels with the row, not the cursor.
        assert!(
            second_row.contains("(current)"),
            "current marker missing from {second_row:?}"
        );
        // Plural-aware role-count copy.
        assert!(first_row.contains("1 role session"));
        assert!(second_row.contains("2 role sessions"));
    }

    #[test]
    fn activate_room_session_restores_role_id_files() {
        let project = fixture();
        let mut roles = BTreeMap::new();
        roles.insert("host".to_owned(), "h1".to_owned());
        roles.insert("qa".to_owned(), "q1".to_owned());
        let room = create_room_session_with_roles(project.path(), roles).unwrap();
        write_session_id(project.path(), "old", "stale").unwrap();

        activate_room_session(project.path(), &room).unwrap();

        assert_eq!(
            read_session_id(project.path(), "host").unwrap().as_deref(),
            Some("h1")
        );
        assert_eq!(
            read_session_id(project.path(), "qa").unwrap().as_deref(),
            Some("q1")
        );
        assert_eq!(read_session_id(project.path(), "old").unwrap(), None);
    }

    #[test]
    fn resolve_room_session_accepts_latest_index_exact_and_prefix() {
        let project = fixture();
        let first = RoomSession {
            id: "room-alpha".to_owned(),
            created_at: "2026-01-01T00:00:00.000Z".to_owned(),
            updated_at: "2026-01-01T00:00:00.000Z".to_owned(),
            role_sessions: BTreeMap::new(),
        };
        let second = RoomSession {
            id: "room-bravo".to_owned(),
            created_at: "2026-01-02T00:00:00.000Z".to_owned(),
            updated_at: "9999-01-01T00:00:00.000Z".to_owned(),
            role_sessions: BTreeMap::new(),
        };
        write_room_session(project.path(), &first).unwrap();
        write_room_session(project.path(), &second).unwrap();

        assert_eq!(
            resolve_room_session(project.path(), "latest")
                .unwrap()
                .map(|s| s.id),
            Some("room-bravo".to_owned())
        );
        assert_eq!(
            resolve_room_session(project.path(), "2")
                .unwrap()
                .map(|s| s.id),
            Some("room-alpha".to_owned())
        );
        assert_eq!(
            resolve_room_session(project.path(), &second.id)
                .unwrap()
                .map(|s| s.id),
            Some("room-bravo".to_owned())
        );
        assert_eq!(
            resolve_room_session(project.path(), "room-b")
                .unwrap()
                .map(|s| s.id),
            Some("room-bravo".to_owned())
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
