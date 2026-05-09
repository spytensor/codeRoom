//! Append-only message bus.
//!
//! [`MessageBus`] is the single source of truth for events emitted by every
//! role in a session. Every [`CrepEvent`] is:
//!
//! 1. Serialized to one line of JSON.
//! 2. Appended to the on-disk log at `.coderoom/messages.jsonl`.
//! 3. Broadcast to all live subscribers (the REPL renderer, future patch
//!    detectors, transcript writers, etc.).
//!
//! Late subscribers do not see historical events; that's the job of
//! [`MessageBus::replay`] (and ultimately `cr show`), which streams the
//! existing on-disk log line-by-line.

use std::path::Path;

use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, broadcast};

use crate::crep::CrepEvent;

/// Capacity of the broadcast ring buffer. Late subscribers that fall this
/// far behind get a `RecvError::Lagged` and skip ahead — they have not
/// missed anything important since the on-disk log is the durable record.
const SUBSCRIBER_CAPACITY: usize = 1024;

/// Append-only event bus.
///
/// Construct one per `cr start` session. Multiple consumers can call
/// [`subscribe`](Self::subscribe) to observe events live; the durable log
/// at the configured path is always the source of truth.
pub struct MessageBus {
    file: Mutex<File>,
    tx: broadcast::Sender<CrepEvent>,
}

impl std::fmt::Debug for MessageBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageBus")
            .field("subscribers", &self.tx.receiver_count())
            .finish_non_exhaustive()
    }
}

impl MessageBus {
    /// Open (or create) the log at `path` and return a fresh bus.
    ///
    /// Existing log content is preserved; new events append after it.
    pub async fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path.as_ref())
            .await?;
        let (tx, _initial) = broadcast::channel(SUBSCRIBER_CAPACITY);
        Ok(Self {
            file: Mutex::new(file),
            tx,
        })
    }

    /// Append the event to the log AND notify subscribers.
    ///
    /// Disk write happens first; if it fails the event is dropped and the
    /// error is returned. Subscribers see only events that successfully
    /// landed on disk, so the on-disk log and the broadcast stream agree.
    pub async fn publish(&self, event: CrepEvent) -> std::io::Result<()> {
        let serialized = serde_json::to_string(&event).map_err(std::io::Error::other)?;
        {
            let mut file = self.file.lock().await;
            file.write_all(serialized.as_bytes()).await?;
            file.write_all(b"\n").await?;
            file.flush().await?;
        }
        // Sending to a broadcast channel with no live receivers returns
        // `Err(SendError)`; that's expected and not a publish failure.
        let _ = self.tx.send(event);
        Ok(())
    }

    /// Subscribe to live events. Late subscribers see only events that
    /// arrive after this call.
    pub fn subscribe(&self) -> broadcast::Receiver<CrepEvent> {
        self.tx.subscribe()
    }

    /// Stream every event currently on disk at `path`, in order, decoding
    /// each line as a [`CrepEvent`]. Lines that fail to parse are skipped
    /// (and a tracing warning is emitted) so a single corrupt line does
    /// not poison the whole replay.
    pub async fn replay(path: impl AsRef<Path>) -> std::io::Result<Vec<CrepEvent>> {
        let file = File::open(path.as_ref()).await?;
        let mut lines = BufReader::new(file).lines();
        let mut out = Vec::new();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<CrepEvent>(&line) {
                Ok(event) => out.push(event),
                Err(error) => tracing::warn!(%error, "skipping malformed JSONL line on replay"),
            }
        }
        Ok(out)
    }

    /// Number of currently-active subscribers. Useful for diagnostics.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crep::StopReason;
    use pretty_assertions::assert_eq;

    fn sample_event(role: &str) -> CrepEvent {
        CrepEvent::RoleStarted {
            role: role.to_owned(),
            engine: "cc".to_owned(),
            model: "claude-opus-4-7".to_owned(),
            session_id: format!("session-{role}"),
            priors_hash: "dh1:0000".to_owned(),
        }
    }

    #[tokio::test]
    async fn publish_appends_line_to_log() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("messages.jsonl");
        let bus = MessageBus::open(&log).await.unwrap();

        bus.publish(sample_event("backend")).await.unwrap();
        bus.publish(sample_event("frontend")).await.unwrap();

        let content = tokio::fs::read_to_string(&log).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let _: CrepEvent =
                serde_json::from_str(line).expect("each line round-trips as CrepEvent");
        }
    }

    #[tokio::test]
    async fn subscribers_receive_published_events() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("messages.jsonl");
        let bus = MessageBus::open(&log).await.unwrap();

        let mut rx_a = bus.subscribe();
        let mut rx_b = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);

        let event = sample_event("backend");
        bus.publish(event.clone()).await.unwrap();

        let recv_a = rx_a.recv().await.expect("subscriber A receives");
        let recv_b = rx_b.recv().await.expect("subscriber B receives");
        assert_eq!(recv_a, event);
        assert_eq!(recv_b, event);
    }

    #[tokio::test]
    async fn open_preserves_existing_content() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("messages.jsonl");

        // First session writes one event then drops the bus.
        {
            let bus = MessageBus::open(&log).await.unwrap();
            bus.publish(sample_event("first")).await.unwrap();
        }

        // Second session opens the same log, writes another event.
        {
            let bus = MessageBus::open(&log).await.unwrap();
            bus.publish(sample_event("second")).await.unwrap();
        }

        let replayed = MessageBus::replay(&log).await.unwrap();
        assert_eq!(replayed.len(), 2);
        match (&replayed[0], &replayed[1]) {
            (
                CrepEvent::RoleStarted { role: r0, .. },
                CrepEvent::RoleStarted { role: r1, .. },
            ) => {
                assert_eq!(r0, "first");
                assert_eq!(r1, "second");
            }
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[tokio::test]
    async fn replay_skips_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("messages.jsonl");

        // Write a mix of valid and broken lines.
        let valid = serde_json::to_string(&sample_event("ok")).unwrap();
        let stopped = serde_json::to_string(&CrepEvent::RoleStopped {
            role: "ok".to_owned(),
            reason: StopReason::Completed,
        })
        .unwrap();
        let mixed = format!("{valid}\nthis-is-not-json\n\n{stopped}\n");
        tokio::fs::write(&log, mixed).await.unwrap();

        let replayed = MessageBus::replay(&log).await.unwrap();
        assert_eq!(replayed.len(), 2);
    }

    #[tokio::test]
    async fn debug_format_does_not_leak_file_internals() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("messages.jsonl");
        let bus = MessageBus::open(&log).await.unwrap();
        let dbg = format!("{bus:?}");
        assert!(dbg.contains("MessageBus"));
        assert!(!dbg.contains("File"), "Debug should not expose tokio::fs::File internals");
    }
}
