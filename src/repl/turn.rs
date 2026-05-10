use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use crossterm::style::Stylize;
use tokio::sync::mpsc;
use tracing::warn;

use crate::adapter::UserMessage;
use crate::crep::CrepEvent;
use crate::output;
use crate::permissions::BridgeRequestSink;

use super::permission_prompt;
use super::render::render_event;
use super::status::{StatusRegion, SPINNER_TICK_MS};

/// Final assistant-turn fields captured during a single role's drain.
#[derive(Debug, Clone)]
pub(super) struct CapturedTurn {
    pub(super) text: String,
    pub(super) mentions: Vec<String>,
}

/// Fold noisy tool events during a live turn. Full details are still
/// persisted in `.coderoom/messages.jsonl`; this keeps the terminal
/// focused on the user's prompt and the role's final answer.
#[derive(Debug, Default)]
pub(super) struct TurnActivity {
    proposed: usize,
    completed: usize,
    failed: usize,
    tools: BTreeMap<String, usize>,
}

impl TurnActivity {
    pub(super) fn from_foldable_event(event: &CrepEvent, active_role: &str) -> Option<Self> {
        match event {
            CrepEvent::ToolCallProposed {
                role, tool_name, ..
            } if role == active_role => {
                let mut tools = BTreeMap::new();
                tools.insert(tool_name.clone(), 1);
                Some(Self {
                    proposed: 1,
                    tools,
                    ..Self::default()
                })
            }
            CrepEvent::ToolCallExecuted { role, ok, .. } if role == active_role => Some(Self {
                completed: 1,
                failed: usize::from(!ok),
                ..Self::default()
            }),
            _ => None,
        }
    }

    pub(super) fn merge_into(self, other: &mut Self) {
        other.proposed += self.proposed;
        other.completed += self.completed;
        other.failed += self.failed;
        for (tool, count) in self.tools {
            *other.tools.entry(tool).or_default() += count;
        }
    }

    pub(super) fn summary_line(&self, role: &str) -> Option<String> {
        if self.proposed == 0 && self.completed == 0 {
            return None;
        }

        let mut parts = self
            .tools
            .iter()
            .take(4)
            .map(|(tool, count)| {
                if *count == 1 {
                    tool.clone()
                } else {
                    format!("{tool}×{count}")
                }
            })
            .collect::<Vec<_>>();
        let hidden = self.tools.len().saturating_sub(parts.len());
        if hidden > 0 {
            parts.push(format!("+{hidden}"));
        }
        let tools = if parts.is_empty() {
            "tools".to_owned()
        } else {
            parts.join(", ")
        };
        let status = if self.failed == 0 {
            format!("{} ok", self.completed)
        } else {
            format!(
                "{} ok, {} failed",
                self.completed - self.failed,
                self.failed
            )
        };
        Some(format!("  @{role} tools folded · {tools} · {status}"))
    }

    fn render_summary(&self, role: &str) {
        if let Some(line) = self.summary_line(role) {
            println!("{}", line.with(output::DIM));
        }
    }
}

/// Send `text` to `role` and drain bus events until that role's turn
/// ends. Returns the captured `RoleSpoke` info, or `None` if the role
/// stopped before producing a `RoleSpoke` (e.g., immediate crash).
///
/// Tool chatter is folded into a one-line live summary; full events are
/// still persisted in the JSONL log. This only returns to the caller
/// once the role's turn boundary is observed.
pub(super) async fn drain_one_turn(
    tx_user: mpsc::Sender<UserMessage>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    bridge_rx: &mut tokio::sync::mpsc::Receiver<BridgeRequestSink>,
    role: &str,
    text: &str,
    host_role: &str,
) -> Result<Option<CapturedTurn>> {
    if let Err(error) = tx_user.send(UserMessage::Prompt(text.to_owned())).await {
        warn!(role, %error, "user-message channel for role closed");
        return Ok(None);
    }

    let mut captured: Option<CapturedTurn> = None;
    let mut activity = TurnActivity::default();
    let mut status = StatusRegion::start(role);
    let mut ticker = tokio::time::interval(Duration::from_millis(SPINNER_TICK_MS));
    // Skip the immediate fire so the spinner doesn't double-redraw on entry.
    ticker.tick().await;
    loop {
        tokio::select! {
            biased;
            // Permission requests are surfaced before role events so the
            // user is asked the moment the engine pauses for approval,
            // not after the next tool trace flushes.
            request = bridge_rx.recv() => {
                let Some(sink) = request else { continue };
                status.clear();
                if let Err(error) = permission_prompt::handle_request(sink, host_role).await {
                    output::bad(format!("permission prompt failed: {error:#}"));
                }
                status.repaint();
            }
            recv = rx.recv() => match recv {
                Ok(event) => {
                    if let Some(hidden) = TurnActivity::from_foldable_event(&event, role) {
                        hidden.merge_into(&mut activity);
                        continue;
                    }

                    status.clear();
                    let done = match &event {
                        CrepEvent::RoleSpoke {
                            role: spoken,
                            text,
                            mentions,
                            ..
                        } if spoken == role => {
                            captured = Some(CapturedTurn {
                                text: text.clone(),
                                mentions: mentions.clone(),
                            });
                            activity.render_summary(role);
                            render_event(&event, host_role);
                            true
                        }
                        CrepEvent::RoleStopped { role: stopped, .. } if stopped == role => {
                            activity.render_summary(role);
                            render_event(&event, host_role);
                            true
                        }
                        _ => {
                            render_event(&event, host_role);
                            false
                        }
                    };
                    if done {
                        break;
                    }
                    status.repaint();
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    status.clear();
                    output::system(format!(
                        "renderer fell behind, skipped {skipped} event(s)"
                    ));
                    status.repaint();
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = ticker.tick() => status.advance(),
        }
    }
    status.clear();
    Ok(captured)
}
