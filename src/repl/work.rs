use std::collections::BTreeMap;
use std::time::Duration;

use crossterm::terminal;

use crate::adapter::cc::parse_mentions;
use crate::crep::CrepEvent;
use crate::output;
use crate::output::work_card::{Step, StepKind, WorkCard, WorkStatus};
use crate::work;

use super::render::summarize_tool_input;

const DEFAULT_CARD_WIDTH: usize = 80;

#[derive(Debug, Clone)]
pub(super) struct TurnWork {
    role: String,
    role_color: crossterm::style::Color,
    title: String,
    title_from_task_block: bool,
    steps: Vec<Step>,
    pending_steps: BTreeMap<String, usize>,
    current_step: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CleanedRoleText {
    pub(super) text: String,
    pub(super) mentions: Vec<String>,
}

impl TurnWork {
    pub(super) fn new(role: &str, host_role: &str, prompt: &str) -> Self {
        Self {
            role: role.to_owned(),
            role_color: output::role_color(role, host_role),
            title: work::fallback_title(prompt),
            title_from_task_block: false,
            steps: Vec::new(),
            pending_steps: BTreeMap::new(),
            current_step: None,
        }
    }

    pub(super) fn apply_event(&mut self, event: &CrepEvent) {
        match event {
            CrepEvent::WorkTitle { role, title } if role == &self.role => {
                self.title.clone_from(title);
                self.title_from_task_block = true;
            }
            CrepEvent::ToolCallProposed {
                role,
                tool_name,
                tool_input,
                tool_use_id,
            } if role == &self.role => {
                let text = step_label(tool_name, tool_input);
                let idx = self.steps.len();
                self.pending_steps.insert(tool_use_id.clone(), idx);
                self.current_step = Some(text.clone());
                self.steps.push(Step {
                    kind: StepKind::Active,
                    text,
                    time: None,
                });
            }
            CrepEvent::ToolCallExecuted {
                role,
                tool_use_id,
                ok,
                output_summary,
            } if role == &self.role => {
                let text = executed_label(*ok, output_summary);
                if let Some(idx) = self.pending_steps.remove(tool_use_id) {
                    if let Some(step) = self.steps.get_mut(idx) {
                        step.kind = StepKind::Done;
                        step.text = if output_summary.trim().is_empty() {
                            step.text.clone()
                        } else {
                            format!("{} · {text}", step.text)
                        };
                    }
                } else {
                    self.steps.push(Step {
                        kind: StepKind::Done,
                        text,
                        time: None,
                    });
                }
                self.current_step = self
                    .steps
                    .iter()
                    .rev()
                    .find(|step| step.kind == StepKind::Active)
                    .map(|step| step.text.clone());
            }
            CrepEvent::PermissionDenied {
                role,
                tool_name,
                reason,
                ..
            } if role == &self.role => {
                if !self.mark_latest_active_denied(tool_name, reason) {
                    let text = format!("{tool_name} denied · {reason}");
                    self.steps.push(Step {
                        kind: StepKind::Done,
                        text,
                        time: None,
                    });
                }
                self.current_step = None;
            }
            _ => {}
        }
    }

    pub(super) fn clean_role_text(&mut self, raw_text: &str) -> CleanedRoleText {
        let extracted = work::extract_cr_task(raw_text);
        if let Some(title) = extracted.title {
            self.title = title;
            self.title_from_task_block = true;
        }
        let text = extracted.body.trim().to_owned();
        let mentions = parse_mentions(&text);
        CleanedRoleText { text, mentions }
    }

    pub(super) fn done_card(&self, duration: Duration) -> WorkCard {
        WorkCard {
            role: self.role.clone(),
            role_color: self.role_color,
            title: self.title.clone(),
            status: WorkStatus::Done {
                duration,
                steps_count: self.steps.len(),
            },
            steps: self.steps.clone(),
            collapsed: true,
        }
    }

    pub(super) fn interrupted_card(&self, reason: impl Into<String>) -> WorkCard {
        WorkCard {
            role: self.role.clone(),
            role_color: self.role_color,
            title: self.title.clone(),
            status: WorkStatus::Interrupted {
                reason: reason.into(),
            },
            steps: self.steps.clone(),
            collapsed: false,
        }
    }

    #[cfg(test)]
    pub(super) fn title_from_task_block(&self) -> bool {
        self.title_from_task_block
    }

    fn mark_latest_active_denied(&mut self, tool_name: &str, reason: &str) -> bool {
        let Some(step) = self
            .steps
            .iter_mut()
            .rev()
            .find(|step| step.kind == StepKind::Active && step_matches_tool(step, tool_name))
        else {
            return false;
        };
        step.kind = StepKind::Done;
        step.text = format!("{} · denied: {reason}", step.text);
        true
    }
}

pub(super) fn render_card(card: &WorkCard) {
    println!("{}", card.render(card_width()));
}

pub(super) fn card_width() -> usize {
    terminal::size().map_or(DEFAULT_CARD_WIDTH, |(cols, _)| usize::from(cols))
}

fn step_label(tool_name: &str, tool_input: &serde_json::Value) -> String {
    let summary = summarize_tool_input(tool_input);
    let prefix = if is_delegate_tool(tool_name) {
        "delegate"
    } else {
        tool_name
    };
    if summary.trim().is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix} {summary}")
    }
}

fn executed_label(ok: bool, output_summary: &str) -> String {
    let status = if ok { "ok" } else { "failed" };
    if output_summary.trim().is_empty() {
        status.to_owned()
    } else {
        format!(
            "{status}: {}",
            output::truncate_visible(output_summary, 120)
        )
    }
}

fn is_delegate_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Task" | "Agent" | "Subagent")
}

fn step_matches_tool(step: &Step, tool_name: &str) -> bool {
    if is_delegate_tool(tool_name) {
        step.text.starts_with("delegate")
    } else {
        step.text.starts_with(tool_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn cr_task_block_is_extracted_and_removed() {
        let input = "```cr-task\nScan repo permissions\n```\n\nHere is the result.";
        let extracted = work::extract_cr_task(input);
        assert_eq!(extracted.title.as_deref(), Some("Scan repo permissions"));
        assert_eq!(extracted.body, "Here is the result.");
    }

    #[test]
    fn malformed_cr_task_block_is_preserved() {
        let input = "```cr-task\nNo close\nbody";
        let extracted = work::extract_cr_task(input);
        assert_eq!(extracted.title, None);
        assert_eq!(extracted.body, input);
    }

    #[test]
    fn cr_task_title_is_capped_to_twenty_words() {
        let input = "```cr-task\none two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty twentyone\n```\nBody";
        let extracted = work::extract_cr_task(input);
        assert_eq!(
            extracted.title.as_deref(),
            Some(
                "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty"
            )
        );
    }

    #[test]
    fn turn_work_tracks_tool_steps() {
        let mut work = TurnWork::new("security", "host", "scan repo");
        work.apply_event(&CrepEvent::ToolCallProposed {
            role: "security".into(),
            tool_name: "Read".into(),
            tool_input: serde_json::json!({"file_path": "README.md"}),
            tool_use_id: "tool-1".into(),
        });
        work.apply_event(&CrepEvent::ToolCallExecuted {
            role: "security".into(),
            tool_use_id: "tool-1".into(),
            ok: true,
            output_summary: "README.md".into(),
        });

        let card = work.done_card(Duration::from_secs(1));
        assert_eq!(card.steps.len(), 1);
        assert_eq!(card.steps[0].kind, StepKind::Done);
        assert!(card.steps[0].text.contains("Read README.md"));
        assert!(card.steps[0].text.contains("ok"));
    }

    #[test]
    fn permission_denied_closes_latest_active_step_when_possible() {
        let mut work = TurnWork::new("security", "host", "scan repo");
        work.apply_event(&CrepEvent::ToolCallProposed {
            role: "security".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "rm -rf target"}),
            tool_use_id: "tool-1".into(),
        });
        work.apply_event(&CrepEvent::PermissionDenied {
            role: "security".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "rm -rf target"}),
            reason: "requires review".into(),
        });

        let card = work.done_card(Duration::from_secs(1));
        assert_eq!(card.steps.len(), 1);
        assert_eq!(card.steps[0].kind, StepKind::Done);
        assert!(card.steps[0].text.contains("denied"));
    }

    #[test]
    fn clean_role_text_updates_title_and_mentions() {
        let mut work = TurnWork::new("security", "host", "fallback");
        let cleaned = work.clean_role_text(
            "```cr-task\nReview auth\n```\n\nI checked with @backend, not @ghost.",
        );
        assert!(work.title_from_task_block());
        assert_eq!(work.title, "Review auth");
        assert_eq!(cleaned.text, "I checked with @backend, not @ghost.");
        assert_eq!(cleaned.mentions, vec!["backend", "ghost"]);
    }
}
