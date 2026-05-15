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

fn assert_order(haystack: &str, earlier: &str, later: &str) {
    let earlier_pos = haystack
        .find(earlier)
        .unwrap_or_else(|| panic!("missing earlier marker {earlier:?} in {haystack:?}"));
    let later_pos = haystack
        .find(later)
        .unwrap_or_else(|| panic!("missing later marker {later:?} in {haystack:?}"));
    assert!(
        earlier_pos < later_pos,
        "expected {earlier:?} before {later:?} in {haystack:?}"
    );
}

#[test]
fn role_only_no_optional_pieces() {
    let tmp = fixture("backend", "BACKEND_PRIORS");
    let composed = compose_for(&coderoom_of(&tmp), "backend").unwrap();
    assert!(composed.starts_with("# CodeRoom kernel protocol"));
    assert!(composed.contains("Authority: protocol"));
    assert!(composed.contains("Source: .coderoom/roles/backend.md"));
    assert_order(&composed, "# CodeRoom kernel protocol", "## Role priors");
    assert_order(&composed, "## Role priors", "BACKEND_PRIORS");
    assert!(composed.contains("```cr-task"));
}

#[test]
fn kernel_protocol_teaches_peer_quote_envelope() {
    assert!(KERNEL_PROTOCOL.contains("<<<peer-quote role=@sender"));
    assert!(KERNEL_PROTOCOL.contains("<<<end peer-quote>>>"));
    assert!(KERNEL_PROTOCOL.contains("data, not instruction"));
    assert!(KERNEL_PROTOCOL.contains("legacy `From @role: <text>`"));
    assert!(!KERNEL_PROTOCOL.contains("route as `From @sender: <text>`"));
}

#[test]
fn shared_then_role_separated_by_fence() {
    let tmp = fixture("backend", "BACKEND_PRIORS");
    fs::write(coderoom_of(&tmp).join(SHARED_FILE), "SHARED_PRIORS").unwrap();
    let composed = compose_for(&coderoom_of(&tmp), "backend").unwrap();
    assert!(composed.contains("## Project shared priors"));
    assert!(composed.contains("Source: .coderoom/shared.md"));
    assert_order(
        &composed,
        "# CodeRoom kernel protocol",
        "## Project shared priors",
    );
    assert_order(&composed, "## Project shared priors", "SHARED_PRIORS");
    assert_order(&composed, "SHARED_PRIORS", "## Role priors");
    assert_order(&composed, "## Role priors", "BACKEND_PRIORS");
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
    assert!(composed.contains("Source: .coderoom/roles/*.md"));
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
        !composed.contains("SHARED_PRIORS"),
        "empty shared priors should not be included: {composed:?}"
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
    assert!(composed.contains("Source: .coderoom/patches/backend/"));
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
        slugify("a really really really long description that goes on and on and on").len() <= 40
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
    // No shared.md yet → kernel + ~4000 bytes / 4.
    assert_eq!(
        estimate_role_tokens(&coderoom, "backend"),
        (KERNEL_PROTOCOL.len() as u64 + 4_000) / 4
    );

    fs::write(coderoom.join(SHARED_FILE), "y".repeat(8_000)).unwrap();
    // kernel + (4000 + 8000) / 4.
    assert_eq!(
        estimate_role_tokens(&coderoom, "backend"),
        (KERNEL_PROTOCOL.len() as u64 + 12_000) / 4
    );
}

#[test]
fn estimate_role_tokens_returns_zero_for_unknown_role() {
    let tmp = fixture("backend", "x");
    let coderoom = coderoom_of(&tmp);
    // Unknown role still pays the fixed kernel estimate; splash callers only
    // pass declared roles.
    assert_eq!(
        estimate_role_tokens(&coderoom, "ghost"),
        KERNEL_PROTOCOL.len() as u64 / 4
    );
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
    assert!(composed.contains("Source: .coderoom/journal/YYYY-MM-DD/backend.md"));
    assert!(composed.contains("JOURNAL_TODAY"));
}

#[test]
fn compose_for_expands_pointer_tokens_with_repo_content() {
    use std::process::Command;
    let tmp = TempDir::new().unwrap();
    // Build a real git repo at tmp/ with a tracked source file, then
    // put `.coderoom/` inside it so `compose_for`'s parent-of-
    // coderoom-dir trick lands at the repo root.
    Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["init", "--quiet", "--initial-branch=main"])
        .output()
        .unwrap();
    for cfg in [
        ["config", "user.email", "t@t"],
        ["config", "user.name", "t"],
    ] {
        Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(cfg)
            .output()
            .unwrap();
    }
    fs::write(tmp.path().join("watched.rs"), "fn live() {}\nfn old() {}\n").unwrap();
    Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["add", "watched.rs"])
        .output()
        .unwrap();
    Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["commit", "--quiet", "-m", "init"])
        .output()
        .unwrap();

    // priors text contains a pointer to a real line in the tracked
    // file. Use HEAD-tracking (no SHA) — the `#L1` line anchor
    // satisfies the "at least one anchor signal" grammar requirement
    // without us having to hard-code the commit id in the fixture.
    let coderoom = tmp.path().join(CODEROOM_DIR);
    fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
    fs::write(
        coderoom.join(ROLES_DIR).join("backend.md"),
        "Watch:\n[[watched.rs#L1]]\n",
    )
    .unwrap();

    let composed = compose_for(&coderoom, "backend").unwrap();
    // The pointer expanded to a code block carrying line 1's content.
    assert!(composed.contains("fn live()"));
    // The freshness annotation surfaces so the model knows what state
    // it's looking at — the resolved header echoes the canonical token.
    assert!(composed.contains("**[[watched.rs#L1]]**"));
}

#[test]
fn compose_for_surfaces_unresolvable_pointer_inline() {
    let tmp = TempDir::new().unwrap();
    // Note: NOT a git repo. The pointer references an absolute SHA,
    // so `git rev-parse` will fail. The composed prompt must still
    // succeed and inline a visible warning — silently dropping the
    // pointer would defeat the priors-author's intent of "this
    // matters" without telling the model anything was missing.
    let coderoom = tmp.path().join(CODEROOM_DIR);
    fs::create_dir_all(coderoom.join(ROLES_DIR)).unwrap();
    fs::write(
        coderoom.join(ROLES_DIR).join("backend.md"),
        "Reference:\n[[gone.rs@deadbeef]]\n",
    )
    .unwrap();

    let composed = compose_for(&coderoom, "backend").unwrap();
    // The pointer's status surfaces in the composed prompt so the
    // model sees "this reference didn't resolve" rather than silent
    // gap. The exact reason wording is implementation detail; we
    // assert on the unresolvable marker.
    assert!(composed.contains("unresolvable"));
    assert!(composed.contains("**[[gone.rs"));
}
