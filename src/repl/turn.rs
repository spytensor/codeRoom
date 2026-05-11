use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::warn;

use crate::adapter::UserMessage;
use crate::crep::CrepEvent;
use crate::output;
use crate::permissions::BridgeRequestSink;

use super::permission_prompt;
use super::render::render_event;
use super::status::{StatusRegion, SPINNER_TICK_MS};
use super::work::{self, TurnWork};

/// Final assistant-turn fields captured during a single role's drain.
#[derive(Debug, Clone)]
pub(super) struct CapturedTurn {
    pub(super) text: String,
    /// Tool-call activity observed during this drain. Used by
    /// `send_and_drain` to gate auto-routing — a turn whose tools were
    /// systematically denied probably produced an ungrounded reply, and
    /// its `@<peer>` mentions should not trigger fresh peer turns.
    pub(super) activity: TurnActivity,
}

/// Fold noisy tool events during a live turn. Full details are still
/// persisted in `.coderoom/messages.jsonl`; this keeps the terminal
/// focused on the user's prompt and the role's final answer.
#[derive(Debug, Default, Clone)]
pub(super) struct TurnActivity {
    pub(super) proposed: usize,
    pub(super) completed: usize,
    pub(super) failed: usize,
    /// Subset of `failed`: tool calls rejected by CodeRoom's permission
    /// hook (cc) or refused by the codex/gemini bridge. Tracked
    /// separately so the grounding gate can distinguish "all tools
    /// denied → role hallucinated a reply" from "tests failed but role
    /// has grounded info to share."
    pub(super) denied: usize,
    pub(super) tools: BTreeMap<String, usize>,
    /// Tool names that hit a permission denial this turn — surfaced in
    /// the suppression message so the user knows what to `/allow`.
    pub(super) denied_tools: BTreeMap<String, usize>,
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
            // Engine-agnostic denial signal. cc emits PermissionDenied
            // alongside its tool_use+tool_result(is_error=true) pair;
            // codex (per the new MCP responder) emits PermissionDenied
            // only — no exec_command_started/completed when the user
            // says deny — so this arm is what makes the grounding gate
            // fire on codex hosts too.
            CrepEvent::PermissionDenied {
                role, tool_name, ..
            } if role == active_role => {
                let mut denied_tools = BTreeMap::new();
                denied_tools.insert(tool_name.clone(), 1);
                Some(Self {
                    denied: 1,
                    denied_tools,
                    ..Self::default()
                })
            }
            _ => None,
        }
    }

    pub(super) fn merge_into(self, other: &mut Self) {
        other.proposed += self.proposed;
        other.completed += self.completed;
        other.failed += self.failed;
        other.denied += self.denied;
        for (tool, count) in self.tools {
            *other.tools.entry(tool).or_default() += count;
        }
        for (tool, count) in self.denied_tools {
            *other.denied_tools.entry(tool).or_default() += count;
        }
    }

    #[cfg(test)]
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

    /// Whether the role's turn looks ungrounded enough that its
    /// `@<peer>` mentions should NOT trigger fresh auto-routes.
    ///
    /// The signal we trust most is permission denials: if cc's hook
    /// rejected (or the codex bridge user said no) on three or more
    /// tools, the role almost certainly fell back to memory and the
    /// "team plan" it just wrote is a guess. A successful tool call
    /// in the same turn means the role had at least some grounded
    /// info to share — don't gate.
    ///
    /// Plain `failed` (test failures, `rg` no-matches, command exit 1)
    /// is grounded information by definition — those tools ran and
    /// produced output. We do NOT treat them as ungrounded.
    pub(super) fn looks_ungrounded(&self) -> bool {
        if self.successful_calls() > 0 {
            return false;
        }
        // No successful calls. Ungrounded only when permission denials
        // — not plain failures — pile up. A failed `cargo test` IS
        // grounded info (the role can reason about which tests broke);
        // a denied `Read` is NOT (the role didn't see the file).
        if self.denied >= 3 {
            return true;
        }
        self.proposed > 0 && self.denied == self.proposed
    }

    /// Tool calls that completed successfully (executed AND ok).
    pub(super) fn successful_calls(&self) -> usize {
        self.completed.saturating_sub(self.failed)
    }

    /// Top-N denied tool names ordered by frequency, for the
    /// suppression hint message.
    pub(super) fn top_denied_tools(&self, n: usize) -> Vec<String> {
        let mut items: Vec<(&String, &usize)> = self.denied_tools.iter().collect();
        items.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        items.into_iter().take(n).map(|(k, _)| k.clone()).collect()
    }
}

/// Send `text` to `role` and drain bus events until that role's turn
/// ends. Returns the captured `RoleSpoke` info, or `None` if the role
/// stopped before producing a `RoleSpoke` (e.g., immediate crash).
///
/// Tool activity updates the live status, prints compact work-card
/// snapshots, and appends terse trace lines. This only returns to the
/// caller once the role's turn boundary is observed.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "turn drain owns user, durable, live, permission, role, and work-state channels"
)]
pub(super) async fn drain_one_turn(
    tx_user: mpsc::Sender<UserMessage>,
    rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    live_rx: &mut tokio::sync::broadcast::Receiver<CrepEvent>,
    bridge_rx: &mut tokio::sync::mpsc::Receiver<BridgeRequestSink>,
    role: &str,
    text: &str,
    host_role: &str,
    work: Arc<Mutex<TurnWork>>,
) -> Result<Option<CapturedTurn>> {
    if let Err(error) = tx_user.send(UserMessage::Prompt(text.to_owned())).await {
        warn!(role, %error, "user-message channel for role closed");
        return Ok(None);
    }

    let verbose_tools = verbose_tools_enabled();

    let mut captured: Option<CapturedTurn> = None;
    let mut activity = TurnActivity::default();
    let turn_started = Instant::now();
    let mut status = StatusRegion::start(role);
    let mut printed_working_card = false;
    let mut stream_filter = StreamFilter::default();
    let mut streamed_rendered_text = String::new();
    let mut pending_delta = String::new();
    // Persistent markdown state for the streaming render path. Mutated
    // by each `render_stream_delta` call so the role badge prints
    // once per turn and code blocks survive chunk boundaries.
    let mut stream_md_state = super::markdown::StreamMarkdownState::fresh();
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
                let request_role = sink.request.role.clone();
                let request_tool = sink.request.tool.clone();
                let request_input = sink.request.input.clone();
                status.mark_waiting_approval(&request_role, &request_tool, &request_input);
                status.clear();
                if let Err(error) = permission_prompt::handle_request(sink, host_role).await {
                    output::bad(format!("permission prompt failed: {error:#}"));
                }
                status.clear_waiting_approval(&request_role);
                status.repaint();
            }
            recv = live_rx.recv() => match recv {
                Ok(event) => {
                    let CrepEvent::RoleOutputDelta {
                        role: delta_role,
                        text_delta,
                        ..
                    } = event
                    else {
                        continue;
                    };
                    if delta_role != role {
                        continue;
                    }
                    if let Some(visible_delta) = stream_filter.push(&text_delta) {
                        pending_delta.push_str(&visible_delta);
                    }
                    if pending_delta.contains('\n') || pending_delta.chars().count() >= 180 {
                        status.clear();
                        if let Some(rendered) = render_stream_delta(
                            role,
                            host_role,
                            &mut pending_delta,
                            &mut stream_md_state,
                        ) {
                            streamed_rendered_text.push_str(&rendered);
                        }
                        status.repaint();
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_skipped)) => {
                    // Live text deltas are lossy by design. The durable
                    // final RoleSpoke still arrives through `rx`, so no
                    // user-visible warning is needed here.
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
            },
            recv = rx.recv() => match recv {
                Ok(event) => {
                    if matches!(&event, CrepEvent::WorkTitle { role: titled, .. } if titled == role)
                    {
                        work.lock()
                            .expect("turn work mutex poisoned")
                            .apply_event(&event);
                        continue;
                    }
                    if let Some(hidden) = TurnActivity::from_foldable_event(&event, role) {
                        let maybe_card = {
                            let mut work = work.lock().expect("turn work mutex poisoned");
                            work.apply_event(&event);
                            if !printed_working_card
                                && matches!(event, CrepEvent::ToolCallProposed { .. })
                            {
                                printed_working_card = true;
                                let frame = status.slots.first().map_or(0, |slot| slot.frame);
                                Some(work.working_card(frame))
                            } else {
                                None
                            }
                        };
                        hidden.merge_into(&mut activity);
                        // Per-tool lines duplicate what the WorkCard
                        // (rendered once on first tool, again on turn
                        // end with all steps) and the live status
                        // spinner already convey. Fold them by default;
                        // `CODEROOM_VERBOSE_TOOLS=1` opts back in to the
                        // full audit stream. `cr show` always replays
                        // the full event log.
                        status.clear();
                        if let Some(card) = maybe_card {
                            work::render_card(&card);
                        }
                        if verbose_tools {
                            render_event(&event, host_role);
                        }
                        status.update_from_event(&event);
                        status.repaint();
                        continue;
                    }

                    let done = match &event {
                        CrepEvent::RoleSpoke {
                            role: spoken,
                            text,
                            cost_usd,
                            cache_read,
                            turn_id,
                            thread_id,
                            mentions: _,
                        } if spoken == role => {
                            let (cleaned, card) = {
                                let mut work = work.lock().expect("turn work mutex poisoned");
                                let cleaned = work.clean_role_text(text);
                                let card = work.done_card(turn_started.elapsed());
                                (cleaned, card)
                            };
                            captured = Some(CapturedTurn {
                                text: cleaned.text.clone(),
                                activity: activity.clone(),
                            });
                            status.clear();
                            if let Some(rendered) = render_stream_delta(
                                role,
                                host_role,
                                &mut pending_delta,
                                &mut stream_md_state,
                            ) {
                                streamed_rendered_text.push_str(&rendered);
                            }
                            if printed_working_card || cleaned.text.trim().is_empty() {
                                work::render_card(&card);
                            }
                            let already_streamed = !streamed_rendered_text.trim().is_empty()
                                && same_streamed_text(&streamed_rendered_text, &cleaned.text);
                            if !already_streamed && !cleaned.text.trim().is_empty() {
                                let rendered = CrepEvent::RoleSpoke {
                                    role: spoken.clone(),
                                    text: cleaned.text,
                                    mentions: cleaned.mentions,
                                    cost_usd: *cost_usd,
                                    cache_read: *cache_read,
                                    turn_id: turn_id.clone(),
                                    thread_id: thread_id.clone(),
                                };
                                render_event(&rendered, host_role);
                            }
                            true
                        }
                        CrepEvent::RoleStopped { role: stopped, .. } if stopped == role => {
                            status.clear();
                            if captured.is_none() {
                                let card = work
                                    .lock()
                                    .expect("turn work mutex poisoned")
                                    .interrupted_card("role stopped before replying");
                                work::render_card(&card);
                            }
                            render_event(&event, host_role);
                            true
                        }
                        // `TurnInterrupted` is a turn boundary in v0.2:
                        // adapters emit it when a user `/halt` or
                        // Ctrl-C cancellation reaches them. The role
                        // process stays alive — only the current turn
                        // ends. Surface partial mentions for the user
                        // (per `docs/v0.2-trust-and-interrupt.md` § H.3
                        // they are NOT auto-routed) and finalize the
                        // WorkCard as Interrupted.
                        CrepEvent::TurnInterrupted {
                            role: interrupted,
                            partial_mentions,
                            ..
                        } if interrupted == role => {
                            status.clear();
                            let card = work
                                .lock()
                                .expect("turn work mutex poisoned")
                                .interrupted_card("halted by user");
                            work::render_card(&card);
                            render_event(&event, host_role);
                            if !partial_mentions.is_empty() {
                                let names = partial_mentions
                                    .iter()
                                    .map(|n| format!("@{n}"))
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                output::hint(format!(
                                    "partial reply mentioned {names} (not dispatched)"
                                ));
                            }
                            true
                        }
                        _ => {
                            status.clear();
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

/// Whether the user opted into the full per-tool trace stream via the
/// `CODEROOM_VERBOSE_TOOLS` env var. Any non-empty value enables it.
/// Checked once per turn-drain so a session can change behavior between
/// turns without restarting the REPL.
fn verbose_tools_enabled() -> bool {
    verbose_from_value(std::env::var("CODEROOM_VERBOSE_TOOLS").ok().as_deref())
}

/// Pure decision so the env-var read can stay a one-liner while the
/// gating semantics ("set + non-empty enables") are unit-testable
/// without touching process-global env state.
fn verbose_from_value(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

fn render_stream_delta(
    role: &str,
    host_role: &str,
    pending: &mut String,
    state: &mut super::markdown::StreamMarkdownState,
) -> Option<String> {
    if pending.trim().is_empty() {
        pending.clear();
        return None;
    }
    let text_delta = pending.clone();
    // Bypass `render_event` (which builds a fresh CrepEvent and would
    // route through a stateless renderer) and call the markdown
    // streaming entry point directly so the persistent state — role
    // badge already emitted, currently-inside-a-fenced-code-block —
    // carries from chunk to chunk within the same turn.
    let width = crossterm::terminal::size().map_or(80, |(cols, _)| usize::from(cols));
    let rendered = super::markdown::render_role_markdown_with_state(
        role,
        host_role,
        &text_delta,
        width,
        state,
    );
    println!("{rendered}");
    pending.clear();
    Some(text_delta)
}

fn same_streamed_text(streamed: &str, final_text: &str) -> bool {
    streamed.trim() == final_text.trim()
}

#[derive(Default)]
struct StreamFilter {
    raw: String,
    visible: String,
}

impl StreamFilter {
    fn push(&mut self, delta: &str) -> Option<String> {
        self.raw.push_str(delta);
        let visible = visible_stream_text(&self.raw)?;
        if visible == self.visible {
            return None;
        }
        let suffix = visible
            .strip_prefix(&self.visible)
            .map_or_else(|| visible.clone(), ToOwned::to_owned);
        self.visible = visible;
        Some(suffix)
    }
}

fn visible_stream_text(raw: &str) -> Option<String> {
    const MARKER: &str = "```cr-task";
    let leading_ws = raw.len() - raw.trim_start().len();
    let after_ws = &raw[leading_ws..];

    if MARKER.starts_with(after_ws) {
        return None;
    }
    if !after_ws.starts_with(MARKER) {
        return Some(raw.to_owned());
    }

    let opening_newline = after_ws.find('\n')?;
    let after_opening = &after_ws[opening_newline + 1..];
    let closing_end = closing_fence_end(after_opening)?;
    let visible = &after_opening[closing_end..];
    Some(visible.trim_start_matches(['\r', '\n']).to_owned())
}

fn closing_fence_end(text: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\r', '\n']).trim();
        if content == "```" {
            return Some(offset + line.len());
        }
        offset += line.len();
    }
    (text.trim() == "```").then_some(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_filter_hides_cr_task_until_body() {
        let mut filter = StreamFilter::default();
        assert_eq!(filter.push("```cr"), None);
        assert_eq!(filter.push("-task\nReview permissions\n"), None);
        assert_eq!(
            filter.push("```\n\nFinding one."),
            Some("Finding one.".to_owned())
        );
        assert_eq!(
            filter.push("\nFinding two."),
            Some("\nFinding two.".to_owned())
        );
    }

    #[test]
    fn stream_filter_passes_normal_text() {
        let mut filter = StreamFilter::default();
        assert_eq!(filter.push("Hello"), Some("Hello".to_owned()));
        assert_eq!(filter.push(" world"), Some(" world".to_owned()));
    }

    #[test]
    fn verbose_gate_unset_means_folded() {
        assert!(!verbose_from_value(None));
    }

    #[test]
    fn verbose_gate_empty_string_means_folded() {
        // An empty value (`CODEROOM_VERBOSE_TOOLS=`) is the same as
        // unset — users sometimes export with no value to "clear" it
        // and we shouldn't surprise them with verbose output.
        assert!(!verbose_from_value(Some("")));
    }

    #[test]
    fn verbose_gate_any_non_empty_value_enables() {
        assert!(verbose_from_value(Some("1")));
        assert!(verbose_from_value(Some("true")));
        assert!(verbose_from_value(Some("yes")));
    }
}
