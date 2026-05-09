//! `cr cost` — per-role spend summary derived from
//! `.coderoom/messages.jsonl`.
//!
//! v0.1 sums `RoleSpoke.cost_usd` by role across the entire log
//! (or, with `--since`, from the given date forward). Cache reads are
//! also surfaced because they're a useful proxy for "how warm was
//! this session" — large `cache_read` totals usually mean low cost
//! per turn.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use chrono::NaiveDate;

use crate::bus::MessageBus;
use crate::config::CODEROOM_DIR;
use crate::crep::CrepEvent;

/// Aggregate stats for a single role.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct RoleStats {
    /// Number of `RoleSpoke` events from this role.
    pub turns: u64,
    /// Sum of `cost_usd` across all turns.
    pub cost_usd: f64,
    /// Sum of `cache_read_input_tokens` across all turns.
    pub cache_read: u64,
}

/// Load the message log and aggregate stats per role.
///
/// `since` is an inclusive lower bound on the (engine-reported) event
/// date. Right now CREP doesn't carry timestamps, so `since` is honored
/// via filename heuristic only — the JSONL's mtime — and effectively
/// means "skip the log if the file's older than `since`". v0.2 will
/// add per-event timestamps; for now `since=None` is the only fully
/// accurate mode and is what `cr cost` uses by default.
pub async fn aggregate(
    project_root: &Path,
    since: Option<NaiveDate>,
) -> Result<BTreeMap<String, RoleStats>> {
    let log_path = project_root.join(CODEROOM_DIR).join("messages.jsonl");
    if !log_path.is_file() {
        return Ok(BTreeMap::new());
    }
    if let Some(since) = since {
        if let Ok(metadata) = std::fs::metadata(&log_path) {
            if let Ok(modified) = metadata.modified() {
                let modified_date = chrono::DateTime::<chrono::Local>::from(modified).date_naive();
                if modified_date < since {
                    return Ok(BTreeMap::new());
                }
            }
        }
    }

    let events = MessageBus::replay(&log_path).await?;
    let mut by_role: BTreeMap<String, RoleStats> = BTreeMap::new();
    for event in events {
        if let CrepEvent::RoleSpoke {
            role,
            cost_usd,
            cache_read,
            ..
        } = event
        {
            let entry = by_role.entry(role).or_default();
            entry.turns += 1;
            entry.cost_usd += cost_usd;
            entry.cache_read = entry.cache_read.saturating_add(cache_read);
        }
    }
    Ok(by_role)
}

/// Top-level entry point for `cr cost`. Loads the log, aggregates,
/// prints a table to stdout.
pub async fn run(project_root: &Path, since: Option<NaiveDate>) -> Result<()> {
    let stats = aggregate(project_root, since).await?;
    if stats.is_empty() {
        println!("(no message log yet — run `cr start` first)");
        return Ok(());
    }

    let total_turns: u64 = stats.values().map(|s| s.turns).sum();
    let total_cost: f64 = stats.values().map(|s| s.cost_usd).sum();
    let total_cache: u64 = stats.values().map(|s| s.cache_read).sum();

    println!(
        "{:<16} {:>6} {:>10} {:>14}",
        "role", "turns", "cost (USD)", "cache_read"
    );
    println!("{}", "-".repeat(50));
    for (role, s) in &stats {
        println!(
            "{:<16} {:>6} {:>10.4} {:>14}",
            format!("@{role}"),
            s.turns,
            s.cost_usd,
            s.cache_read
        );
    }
    println!("{}", "-".repeat(50));
    println!(
        "{:<16} {:>6} {:>10.4} {:>14}",
        "TOTAL", total_turns, total_cost, total_cache
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crep::StopReason;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    fn write_log(tmp: &TempDir, events: &[CrepEvent]) -> std::path::PathBuf {
        let coderoom = tmp.path().join(CODEROOM_DIR);
        std::fs::create_dir_all(&coderoom).unwrap();
        let log = coderoom.join("messages.jsonl");
        let mut body = String::new();
        for e in events {
            body.push_str(&serde_json::to_string(e).unwrap());
            body.push('\n');
        }
        std::fs::write(&log, body).unwrap();
        log
    }

    fn spoke(role: &str, cost: f64, cache: u64) -> CrepEvent {
        CrepEvent::RoleSpoke {
            role: role.into(),
            text: "x".into(),
            mentions: Vec::new(),
            cost_usd: cost,
            cache_read: cache,
        }
    }

    #[tokio::test]
    async fn aggregate_sums_cost_and_cache_per_role() {
        let tmp = TempDir::new().unwrap();
        write_log(
            &tmp,
            &[
                spoke("backend", 0.05, 1000),
                spoke("backend", 0.10, 2000),
                spoke("frontend", 0.02, 500),
                CrepEvent::RoleStopped {
                    role: "backend".into(),
                    reason: StopReason::Completed,
                },
            ],
        );
        let stats = aggregate(tmp.path(), None).await.unwrap();
        assert_eq!(stats.len(), 2);
        let backend = stats.get("backend").unwrap();
        assert_eq!(backend.turns, 2);
        assert!((backend.cost_usd - 0.15).abs() < 1e-9);
        assert_eq!(backend.cache_read, 3_000);
        let frontend = stats.get("frontend").unwrap();
        assert_eq!(frontend.turns, 1);
    }

    #[tokio::test]
    async fn aggregate_empty_when_no_log() {
        let tmp = TempDir::new().unwrap();
        let stats = aggregate(tmp.path(), None).await.unwrap();
        assert!(stats.is_empty());
    }

    #[tokio::test]
    async fn aggregate_skips_non_role_spoke_events() {
        let tmp = TempDir::new().unwrap();
        write_log(
            &tmp,
            &[
                CrepEvent::RoleStarted {
                    role: "backend".into(),
                    engine: "cc".into(),
                    model: "opus".into(),
                    session_id: "x".into(),
                    priors_hash: "h".into(),
                },
                CrepEvent::RoleStopped {
                    role: "backend".into(),
                    reason: StopReason::Completed,
                },
            ],
        );
        let stats = aggregate(tmp.path(), None).await.unwrap();
        assert!(stats.is_empty());
    }
}
