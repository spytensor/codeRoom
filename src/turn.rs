//! Turn and thread ids for CREP.
//!
//! v0.2 amends [`crate::crep::CrepEvent`] so every turn-scoped event
//! carries two opaque ids:
//!
//! - **`turn_id`** disambiguates events when one role has multiple turns
//!   in flight (queued, parallel auto-route + manual user dispatch).
//! - **`thread_id`** stays constant across the auto-routing chain so
//!   replay and future parallel fan-out surfaces can group related turns
//!   without reconstructing the chain from `parent_turn_id` ancestry.
//!
//! v0.1-shaped events that predate the turn-id world serialize the
//! [`LEGACY_TURN_ID`] empty string. Renderers treat that as "this event
//! has no turn attribution"; replay tools collapse it into a synthetic
//! per-role turn for backward compat.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Stable id type for both turn and thread ids. Renderers and the JSONL
/// log treat it as opaque text.
pub type TurnId = String;

/// Empty-string id used by v0.1-shaped events that predate the
/// turn-id world. Always serialized; renderers fall back to
/// "no turn known" when they see it.
pub const LEGACY_TURN_ID: &str = "";

/// Whether `id` is the legacy empty-string sentinel.
#[must_use]
pub fn is_legacy(id: &str) -> bool {
    id == LEGACY_TURN_ID
}

static TURN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a new opaque turn id. Format is `tu-<hex millis>-<hex counter>`,
/// monotonic within a single `cr` process and grep-friendly in the JSONL
/// log. Cross-process uniqueness is not required (each `cr` REPL is its
/// own session).
///
/// Prefix `tu-` is deliberately distinct from `th-` (thread ids) so a
/// pattern like `\btu-` does not match thread ids and vice versa — a
/// `\bt-` grep would have hit both, which obscures the wire shape in
/// logs.
#[must_use]
pub fn new_turn_id() -> TurnId {
    new_id_with_prefix("tu")
}

/// Generate a new opaque thread id. Same shape as a turn id but with a
/// distinct prefix (`th-`). See [`new_turn_id`] for the rationale.
#[must_use]
pub fn new_thread_id() -> TurnId {
    new_id_with_prefix("th")
}

fn new_id_with_prefix(prefix: &str) -> TurnId {
    let count = TURN_COUNTER.fetch_add(1, Ordering::Relaxed);
    #[allow(clippy::cast_possible_truncation)]
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    format!("{prefix}-{millis:x}-{count:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_ids_are_unique_and_prefixed() {
        let a = new_turn_id();
        let b = new_turn_id();
        assert_ne!(a, b);
        assert!(a.starts_with("tu-"));
        assert!(b.starts_with("tu-"));
    }

    #[test]
    fn thread_ids_use_distinct_prefix() {
        let turn = new_turn_id();
        let thread = new_thread_id();
        assert!(thread.starts_with("th-"));
        assert!(turn.starts_with("tu-"));
        // The two prefixes are disjoint: a grep for one cannot accidentally
        // match the other. (`t-` would have matched both before; that's the
        // collision the prefix change closes.)
        assert!(!thread.starts_with("tu-"));
        assert!(!turn.starts_with("th-"));
    }

    #[test]
    fn legacy_id_round_trips_via_helper() {
        assert!(is_legacy(LEGACY_TURN_ID));
        assert!(is_legacy(""));
        assert!(!is_legacy(&new_turn_id()));
        assert!(!is_legacy(&new_thread_id()));
    }
}
