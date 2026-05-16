use std::path::Path;

use anyhow::Result;

use crate::bus::MessageBus;
use crate::config::{Config, CODEROOM_DIR};
use crate::crep::CrepEvent;
use crate::output;
use crate::work;

use super::render::render_event;

/// Filters applied by `cr show` while replaying `.coderoom/messages.jsonl`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShowOptions {
    /// Role name to render. Stored without a leading `@`.
    pub role: Option<String>,
    /// Skip the log entirely if its filesystem mtime is older than this date.
    ///
    /// CREP v0.1 events do not carry per-event timestamps, so this mirrors
    /// `cr cost --since` until timestamped events land.
    pub since: Option<chrono::NaiveDate>,
    /// Render only the last N matching events.
    pub tail: Option<usize>,
}

/// Replay events in `.coderoom/messages.jsonl` through the same renderer
/// the live REPL uses. Used by `cr show`.
pub async fn show_log(project_root: &Path, options: &ShowOptions) -> Result<()> {
    let coderoom_dir = project_root.join(CODEROOM_DIR);
    let log_path = coderoom_dir.join("messages.jsonl");
    if !log_path.is_file() {
        println!("(no messages — has `cr start` ever run in this project?)");
        return Ok(());
    }
    if let Some(since) = options.since {
        let modified = tokio::fs::metadata(&log_path).await?.modified()?;
        let modified: chrono::DateTime<chrono::Local> = modified.into();
        if modified.date_naive() < since {
            println!("(message log is older than {since})");
            return Ok(());
        }
    }
    // Loading config gives us the host role for stable lavender rendering.
    // If the config can't load (e.g. malformed), fall back to the default
    // host name — the replay still renders, lavender just won't pin.
    let host_role =
        Config::load(project_root).map_or_else(|_| "host".to_owned(), |cfg| cfg.host_role);
    let replay = MessageBus::replay(&log_path).await?;
    if replay.skipped_malformed > 0 {
        output::warn(format!(
            "{} corrupted line(s) skipped while replaying{}",
            replay.skipped_malformed,
            replay
                .first_malformed_line
                .map_or_else(String::new, |line| format!(" (first at line {line})"))
        ));
    }
    if replay.events.is_empty() {
        println!("(message log is empty)");
        return Ok(());
    }
    let events = filter_show_events(&replay.events, options);
    if events.is_empty() {
        println!("(no matching events)");
        return Ok(());
    }
    for event in events {
        render_show_event(event, &host_role);
    }
    Ok(())
}

pub(super) fn filter_show_events<'a>(
    events: &'a [CrepEvent],
    options: &ShowOptions,
) -> Vec<&'a CrepEvent> {
    let mut filtered = events
        .iter()
        .filter(|event| match options.role.as_deref() {
            Some(role) => event_role(event) == role,
            None => true,
        })
        .collect::<Vec<_>>();

    if let Some(tail) = options.tail {
        let keep_from = filtered.len().saturating_sub(tail);
        filtered = filtered.split_off(keep_from);
    }

    filtered
}

fn event_role(event: &CrepEvent) -> &str {
    match event {
        CrepEvent::RoleStarted { role, .. }
        | CrepEvent::RoleSessionUpdated { role, .. }
        | CrepEvent::TurnDispatched { role, .. }
        | CrepEvent::WorkTitle { role, .. }
        | CrepEvent::RoleSpoke { role, .. }
        | CrepEvent::RoleOutputDelta { role, .. }
        | CrepEvent::TurnInterrupted { role, .. }
        | CrepEvent::ToolCallProposed { role, .. }
        | CrepEvent::ToolCallExecuted { role, .. }
        | CrepEvent::PermissionDenied { role, .. }
        | CrepEvent::RoleStopped { role, .. } => role,
    }
}

fn render_show_event(event: &CrepEvent, host_role: &str) {
    for event in normalize_show_event(event) {
        render_event(&event, host_role);
    }
}

pub(super) fn normalize_show_event(event: &CrepEvent) -> Vec<CrepEvent> {
    let CrepEvent::RoleSpoke {
        role,
        text,
        mentions,
        cost_usd,
        cache_read,
        turn_id,
        thread_id,
        outcome,
    } = event
    else {
        return vec![event.clone()];
    };

    let extracted = work::extract_cr_task(text);
    let mut events = Vec::new();
    if let Some(title) = extracted.title {
        events.push(CrepEvent::WorkTitle {
            role: role.clone(),
            title,
            turn_id: turn_id.clone(),
            thread_id: thread_id.clone(),
        });
    }
    let body = extracted.body.trim().to_owned();
    if !body.is_empty() {
        events.push(CrepEvent::RoleSpoke {
            role: role.clone(),
            text: body,
            mentions: mentions.clone(),
            cost_usd: *cost_usd,
            cache_read: *cache_read,
            turn_id: turn_id.clone(),
            thread_id: thread_id.clone(),
            outcome: *outcome,
        });
    }
    events
}
