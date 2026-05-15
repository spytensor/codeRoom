//! Tool permission policy and Claude Code hook bridge.
//!
//! CodeRoom keeps the runtime policy deliberately small at v0.2:
//! `/allow TOOL` and `/deny TOOL` update a session JSON file, and
//! hook-backed adapters read that file on every proposed tool call.
//!
//! When the live REPL is running, hook-backed adapters can additionally
//! reach the user via the [`bridge`] module — a Unix-domain-socket
//! request/response protocol that turns the engine's "ask the user"
//! state into an actual prompt instead of a silent deny.

pub mod bridge;

use std::collections::BTreeSet;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapter::PermissionMode;
use crate::config::CODEROOM_DIR;

pub use bridge::{
    BridgeError, BridgeHandle, BridgeRequest, BridgeRequestSink, BridgeResponse, DecisionScope,
    PermissionDecision, BRIDGE_ENV_VAR,
};

/// Session-local permission overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPolicy {
    /// Tool names explicitly allowed by `/allow`.
    #[serde(default)]
    pub allow: BTreeSet<String>,
    /// Tool names explicitly denied by `/deny`.
    #[serde(default)]
    pub deny: BTreeSet<String>,
}

impl PermissionPolicy {
    /// Load policy from disk, returning an empty policy when the file is
    /// absent or blank.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => Ok(Self::default()),
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing permission policy {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => {
                Err(error).with_context(|| format!("reading permission policy {}", path.display()))
            }
        }
    }

    /// Persist policy to disk, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self).context("serializing permission policy")?;
        std::fs::write(path, format!("{text}\n"))
            .with_context(|| format!("writing permission policy {}", path.display()))
    }

    /// Add an explicit allow and remove any matching explicit deny.
    pub fn allow_tool(&mut self, tool: &str) {
        let tool = canonical_tool_name(tool);
        self.deny.remove(&tool);
        self.allow.insert(tool);
    }

    /// Add an explicit deny and remove any matching explicit allow.
    pub fn deny_tool(&mut self, tool: &str) {
        let tool = canonical_tool_name(tool);
        self.allow.remove(&tool);
        self.deny.insert(tool);
    }

    fn allows(&self, tool: &str) -> bool {
        self.allow.contains(tool)
    }

    fn denies(&self, tool: &str) -> bool {
        self.deny.contains(tool)
    }

    /// Whether this policy has no explicit decisions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }

    /// Human-readable summary for startup and `/permissions`.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        if !self.allow.is_empty() {
            parts.push(format!(
                "allow: {}",
                self.allow.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if !self.deny.is_empty() {
            parts.push(format!(
                "deny: {}",
                self.deny.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        Some(parts.join("; "))
    }

    /// Return an explicit session decision for `tool`, if one exists.
    /// Deny wins over allow, matching the hook-side policy resolver.
    #[must_use]
    pub fn decision_for_tool(&self, tool: &str) -> Option<PermissionDecision> {
        let tool = canonical_tool_name(tool);
        if self.denies(&tool) {
            Some(PermissionDecision::Deny)
        } else if self.allows(&tool) {
            Some(PermissionDecision::Allow)
        } else {
            None
        }
    }
}

/// Path to the session permission policy file under a project root.
#[must_use]
pub fn policy_path(project_root: &Path) -> PathBuf {
    project_root
        .join(CODEROOM_DIR)
        .join("permission_policy.json")
}

/// Path to the session permission policy file under `.coderoom/`.
#[must_use]
pub fn policy_path_for_coderoom(coderoom_dir: &Path) -> PathBuf {
    coderoom_dir.join("permission_policy.json")
}

/// Load, mutate, and save a permission policy file.
pub fn update_policy(
    path: &Path,
    update: impl FnOnce(&mut PermissionPolicy),
) -> Result<PermissionPolicy> {
    let mut policy = PermissionPolicy::load(path)?;
    update(&mut policy);
    policy.save(path)?;
    Ok(policy)
}

/// Clear every explicit decision from a permission policy file.
pub fn clear_policy(path: &Path) -> Result<PermissionPolicy> {
    let policy = PermissionPolicy::default();
    policy.save(path)?;
    Ok(policy)
}

/// Remember a bridge response in the session policy when the user chose
/// session scope. Returns `Ok(None)` for once-only responses.
pub fn update_policy_from_bridge_response(
    path: &Path,
    tool: &str,
    response: &BridgeResponse,
) -> Result<Option<PermissionPolicy>> {
    if !matches!(response.scope, DecisionScope::Session) {
        return Ok(None);
    }

    update_policy(path, |policy| match response.decision {
        PermissionDecision::Allow => policy.allow_tool(tool),
        PermissionDecision::Deny => policy.deny_tool(tool),
    })
    .map(Some)
}

/// Run the hidden Claude Code hook command.
///
/// The hook reads Claude's PreToolUse JSON from stdin and writes the
/// `hookSpecificOutput` JSON that Claude expects on stdout.
///
/// When the verdict is `ask` and the `CODEROOM_PERMISSION_SOCKET`
/// environment variable points at a live REPL bridge socket, the hook
/// promotes the verdict to a real user prompt over that socket. If the
/// bridge is missing or fails, the hook degrades to deny — never
/// allow — so a broken IPC path can never silently authorize a tool.
pub fn run_claude_hook(mode: PermissionMode, policy_file: Option<&Path>) -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("reading Claude hook stdin")?;
    let role = std::env::var(BRIDGE_ROLE_ENV).ok();
    let output =
        claude_hook_output(mode, policy_file, role.as_deref(), &input).unwrap_or_else(|error| {
            claude_hook_output_json(
                "deny",
                &format!("CodeRoom permission hook failed: {error:#}"),
            )
        });
    std::io::stdout()
        .write_all(output.as_bytes())
        .context("writing Claude hook decision")?;
    std::io::stdout()
        .write_all(b"\n")
        .context("writing Claude hook newline")?;
    Ok(())
}

/// Environment variable carrying the role name into the hook subprocess.
/// Set per-role by the cc adapter so the bridge prompt can attribute the
/// request to the right role color.
pub const BRIDGE_ROLE_ENV: &str = "CODEROOM_PERMISSION_ROLE";

fn claude_hook_output(
    mode: PermissionMode,
    policy_file: Option<&Path>,
    role: Option<&str>,
    input: &str,
) -> Result<String> {
    let hook_input: Value = serde_json::from_str(input).context("parsing Claude hook stdin")?;
    let policy = match policy_file {
        Some(path) => PermissionPolicy::load(path)?,
        None => PermissionPolicy::default(),
    };
    let request = ToolRequest::from_hook_input(&hook_input);
    let mut verdict = decide_tool(mode, &policy, &request);

    if verdict.claude_decision == "ask" {
        match bridge::request_decision_blocking(
            role.unwrap_or("?"),
            &request.name,
            &request.input,
            &verdict.reason,
        ) {
            Ok(response) => {
                if let Some(path) = policy_file {
                    let _ = update_policy_from_bridge_response(path, &request.name, &response);
                }
                verdict = match response.decision {
                    PermissionDecision::Allow => allow(format!(
                        "{} approved by user{}",
                        request.name,
                        match response.scope {
                            DecisionScope::Once => "",
                            DecisionScope::Session => " (session)",
                        }
                    )),
                    PermissionDecision::Deny => deny(format!(
                        "{} denied by user{}",
                        request.name,
                        match response.scope {
                            DecisionScope::Once => "",
                            DecisionScope::Session => " (session)",
                        }
                    )),
                };
            }
            Err(BridgeError::NoSocket) => {
                verdict = deny(format!(
                    "{} requires user approval but no live CodeRoom session is available",
                    request.name
                ));
            }
            Err(other) => {
                verdict = deny(format!(
                    "{} denied — CodeRoom permission bridge failed: {other}",
                    request.name
                ));
            }
        }
    }

    Ok(claude_hook_output_json(
        verdict.claude_decision,
        &verdict.reason,
    ))
}

fn claude_hook_output_json(permission_decision: &str, reason: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": permission_decision,
            "permissionDecisionReason": reason,
        }
    })
    .to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolRequest {
    name: String,
    input: Value,
}

impl ToolRequest {
    fn from_hook_input(input: &Value) -> Self {
        let name = input
            .get("tool_name")
            .or_else(|| input.get("toolName"))
            .or_else(|| input.pointer("/tool/name"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let tool_input = input
            .get("tool_input")
            .or_else(|| input.get("toolInput"))
            .or_else(|| input.pointer("/tool/input"))
            .cloned()
            .unwrap_or(Value::Null);
        Self {
            name: canonical_tool_name(&name),
            input: tool_input,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolVerdict {
    claude_decision: &'static str,
    reason: String,
}

fn decide_tool(
    mode: PermissionMode,
    policy: &PermissionPolicy,
    request: &ToolRequest,
) -> ToolVerdict {
    if policy.denies(&request.name) {
        return deny(format!(
            "{} denied by CodeRoom session policy",
            request.name
        ));
    }
    if policy.allows(&request.name) {
        return allow(format!(
            "{} allowed by CodeRoom session policy",
            request.name
        ));
    }

    match mode {
        PermissionMode::Bypass => allow("permission_mode=bypass".to_owned()),
        PermissionMode::Ask => ask(format!(
            "{} requires approval under permission_mode=ask",
            request.name
        )),
        PermissionMode::Auto if is_low_risk_tool(&request.name, &request.input) => {
            allow(format!("{} allowed by permission_mode=auto", request.name))
        }
        PermissionMode::Auto => ask(format!(
            "{} requires approval under permission_mode=auto",
            request.name
        )),
    }
}

fn allow(reason: String) -> ToolVerdict {
    ToolVerdict {
        claude_decision: "allow",
        reason,
    }
}

fn ask(reason: String) -> ToolVerdict {
    ToolVerdict {
        claude_decision: "ask",
        reason,
    }
}

fn deny(reason: String) -> ToolVerdict {
    ToolVerdict {
        claude_decision: "deny",
        reason,
    }
}

fn is_low_risk_tool(tool: &str, input: &Value) -> bool {
    match tool {
        "Glob" | "Grep" | "LS" | "Read" | "TodoRead" => true,
        "Bash" => bash_command_is_read_only(input),
        _ => false,
    }
}

fn bash_command_is_read_only(input: &Value) -> bool {
    let Some(command) = input.get("command").and_then(Value::as_str) else {
        return false;
    };
    let trimmed = command.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|ch| matches!(ch, '\n' | ';' | '>' | '<' | '|' | '&' | '`' | '$'))
    {
        return false;
    }
    let first = trimmed.split_whitespace().next().unwrap_or_default();
    matches!(first, "cat" | "head" | "ls" | "pwd" | "rg" | "tail" | "wc")
}

fn canonical_tool_name(tool: &str) -> String {
    tool.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn ask_mode_asks_for_unknown_tools() {
        let request = ToolRequest {
            name: "Bash".into(),
            input: json!({"command": "cargo test"}),
        };
        let verdict = decide_tool(PermissionMode::Ask, &PermissionPolicy::default(), &request);
        assert_eq!(verdict.claude_decision, "ask");
        assert!(verdict.reason.contains("permission_mode=ask"));
    }

    #[test]
    fn auto_allows_read_only_tools() {
        let request = ToolRequest {
            name: "Read".into(),
            input: json!({"file_path": "README.md"}),
        };
        let verdict = decide_tool(PermissionMode::Auto, &PermissionPolicy::default(), &request);
        assert_eq!(verdict.claude_decision, "allow");
    }

    #[test]
    fn auto_asks_for_mutating_tools() {
        let request = ToolRequest {
            name: "Edit".into(),
            input: json!({"file_path": "src/main.rs"}),
        };
        let verdict = decide_tool(PermissionMode::Auto, &PermissionPolicy::default(), &request);
        assert_eq!(verdict.claude_decision, "ask");
    }

    #[test]
    fn auto_asks_for_bash_commands_with_mutating_subcommands_or_shell_meta() {
        for command in [
            "git push",
            "git reset --hard origin/main",
            "sed -i 's/a/b/' src/main.rs",
            "find . -delete",
            "find . -exec rm {} \\;",
            "cat README.md > copy.md",
            "ls && rm -rf target",
            "ls $(touch owned)",
        ] {
            let request = ToolRequest {
                name: "Bash".into(),
                input: json!({"command": command}),
            };
            let verdict = decide_tool(PermissionMode::Auto, &PermissionPolicy::default(), &request);
            assert_eq!(
                verdict.claude_decision, "ask",
                "{command} should not auto-allow"
            );
        }
    }

    #[test]
    fn auto_allows_simple_read_only_bash_commands() {
        for command in ["pwd", "ls -la", "rg PermissionMode src", "cat README.md"] {
            let request = ToolRequest {
                name: "Bash".into(),
                input: json!({"command": command}),
            };
            let verdict = decide_tool(PermissionMode::Auto, &PermissionPolicy::default(), &request);
            assert_eq!(
                verdict.claude_decision, "allow",
                "{command} should auto-allow"
            );
        }
    }

    #[test]
    fn explicit_deny_wins_over_allow() {
        let mut policy = PermissionPolicy::default();
        policy.allow_tool("Bash");
        policy.deny_tool("Bash");
        let request = ToolRequest {
            name: "Bash".into(),
            input: json!({"command": "pwd"}),
        };
        let verdict = decide_tool(PermissionMode::Bypass, &policy, &request);
        assert_eq!(verdict.claude_decision, "deny");
    }

    #[test]
    fn hook_output_uses_claude_shape() {
        let input = json!({
            "tool_name": "Read",
            "tool_input": {"file_path": "README.md"}
        });
        let out = claude_hook_output(PermissionMode::Auto, None, None, &input.to_string()).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "allow");
    }

    #[test]
    fn invalid_hook_input_defaults_to_deny() {
        let out = claude_hook_output(PermissionMode::Auto, None, None, "not-json").unwrap_err();
        assert!(out.to_string().contains("parsing Claude hook stdin"));

        let fallback =
            claude_hook_output_json("deny", &format!("CodeRoom permission hook failed: {out:#}"));
        let parsed: Value = serde_json::from_str(&fallback).unwrap();
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(parsed["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("CodeRoom permission hook failed"));
    }

    #[test]
    fn policy_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("policy.json");
        let saved = update_policy(&path, |policy| {
            policy.allow_tool("Read");
            policy.deny_tool("Bash");
        })
        .unwrap();
        let loaded = PermissionPolicy::load(&path).unwrap();
        assert_eq!(saved, loaded);
        assert!(loaded.allow.contains("Read"));
        assert!(loaded.deny.contains("Bash"));
    }

    #[test]
    fn policy_summary_surfaces_active_decisions() {
        let mut policy = PermissionPolicy::default();
        assert!(policy.summary().is_none());

        policy.allow_tool("Write");
        policy.allow_tool("Read");
        policy.deny_tool("Bash");

        assert_eq!(
            policy.summary().as_deref(),
            Some("allow: Read, Write; deny: Bash")
        );
    }

    #[test]
    fn clear_policy_removes_saved_decisions() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("policy.json");
        update_policy(&path, |policy| {
            policy.allow_tool("Read");
            policy.deny_tool("Bash");
        })
        .unwrap();

        clear_policy(&path).unwrap();

        let loaded = PermissionPolicy::load(&path).unwrap();
        assert!(loaded.is_empty());
        assert!(loaded.summary().is_none());
    }

    #[test]
    fn policy_decision_canonicalizes_tool_and_denies_win() {
        let mut policy = PermissionPolicy::default();
        policy.allow_tool(" Bash ");
        assert_eq!(
            policy.decision_for_tool("Bash"),
            Some(PermissionDecision::Allow)
        );

        policy.deny_tool("Bash");
        assert_eq!(
            policy.decision_for_tool(" Bash "),
            Some(PermissionDecision::Deny)
        );
    }

    #[test]
    fn bridge_session_response_updates_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("policy.json");
        let response = BridgeResponse {
            v: 1,
            decision: PermissionDecision::Allow,
            scope: DecisionScope::Session,
            reason: "ok".into(),
        };

        let updated = update_policy_from_bridge_response(&path, "Bash", &response)
            .unwrap()
            .expect("session decisions are persisted");
        assert_eq!(
            updated.decision_for_tool("Bash"),
            Some(PermissionDecision::Allow)
        );
    }

    #[test]
    fn bridge_once_response_does_not_update_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("policy.json");
        let response = BridgeResponse::deny("no");

        let updated = update_policy_from_bridge_response(&path, "Bash", &response).unwrap();
        assert!(updated.is_none());
        assert!(!path.exists());
    }
}
