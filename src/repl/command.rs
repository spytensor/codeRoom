/// Static metadata for a slash command, used by the input line editor
/// to offer autocomplete (ghost text + Tab cycle) and — eventually — a
/// dropdown menu. The list is the single source of truth for
/// discoverable commands; [`parse_line`] still owns the dispatch logic.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SlashCommand {
    /// Bare command name, no leading `/`. Used for prefix matching.
    pub(crate) name: &'static str,
    /// One-line description shown in completion menus. Read by the
    /// dropdown menu work in #97 — silenced here because the slash
    /// table is loaded at compile-time and the only consumer of the
    /// description today is the `slash_commands_have_descriptions`
    /// test that guards the data invariant.
    #[allow(dead_code, reason = "consumed by the dropdown menu in #97")]
    pub(crate) description: &'static str,
    /// Whether the command takes any argument. Completion inserts a
    /// trailing space when `true` so the cursor lands ready for the
    /// argument; arg-less commands accept with no trailing whitespace.
    /// `/halt` counts as taking an argument even though the arg is
    /// optional — typing `/halt` then Enter still works because the
    /// parser trims the trailing space.
    pub(crate) takes_args: bool,
}

/// Slash commands available in the REPL, sorted alphabetically for
/// stable Tab-cycle order. Mirrors the dispatch arms in [`parse_line`];
/// the two must stay in sync.
pub(crate) const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "allow",
        description: "allow a tool for the session",
        takes_args: true,
    },
    SlashCommand {
        name: "compact",
        description: "compact live engine context for a role",
        takes_args: true,
    },
    SlashCommand {
        name: "deny",
        description: "deny a tool for the session",
        takes_args: true,
    },
    SlashCommand {
        name: "exit",
        description: "leave the REPL",
        takes_args: false,
    },
    SlashCommand {
        name: "halt",
        description: "interrupt the current turn (optionally @role)",
        // Bare `/halt` (halt every running role) is the common path;
        // `/halt @role` is a refinement the user types out explicitly.
        // Accepting without a trailing space lets the user hit Enter
        // immediately for the unscoped halt.
        takes_args: false,
    },
    SlashCommand {
        name: "help",
        description: "show help",
        takes_args: false,
    },
    SlashCommand {
        name: "host",
        description: "swap host role for this session",
        takes_args: true,
    },
    SlashCommand {
        name: "journal",
        description: "ask a role to write a journal entry",
        takes_args: true,
    },
    SlashCommand {
        name: "patch",
        description: "save a session-time correction to a role's priors",
        takes_args: true,
    },
    SlashCommand {
        name: "quit",
        description: "leave the REPL (alias for /exit)",
        takes_args: false,
    },
    SlashCommand {
        name: "refresh",
        description: "re-instantiate a role with latest priors",
        takes_args: true,
    },
    SlashCommand {
        name: "resume",
        description: "list or switch saved room sessions",
        takes_args: true,
    },
    SlashCommand {
        name: "stop",
        description: "terminate a role's subprocess",
        takes_args: true,
    },
    SlashCommand {
        name: "transcript",
        description: "show recent transcript for a role",
        takes_args: true,
    },
    SlashCommand {
        name: "welcome",
        description: "re-show the first-run welcome card",
        takes_args: false,
    },
];

#[cfg(test)]
mod slash_table_tests {
    use super::{parse_line, Command, SLASH_COMMANDS};

    #[test]
    fn slash_commands_sorted_alphabetically() {
        // The Tab-cycle order is the declaration order. Sorting
        // alphabetically gives users a predictable scan.
        let names: Vec<&str> = SLASH_COMMANDS.iter().map(|c| c.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn slash_commands_have_descriptions() {
        // Every entry must carry a non-empty description: the dropdown
        // menu (issue #97) renders these as the secondary column, and a
        // blank entry would just be visual noise.
        for cmd in SLASH_COMMANDS {
            assert!(
                !cmd.description.trim().is_empty(),
                "/{} missing description",
                cmd.name
            );
        }
    }

    #[test]
    fn every_slash_command_dispatches_to_a_real_arm() {
        // The table mirrors `parse_line`'s dispatch arms. A typo or
        // forgotten arm would silently route the user to `/help`,
        // which is what the unknown-command fallthrough emits — exactly
        // the failure mode this test guards against. `/help` itself is
        // legitimately `Command::Help`, so skip it.
        for cmd in SLASH_COMMANDS {
            if cmd.name == "help" {
                continue;
            }
            // Two-token sample so commands that need both a role and
            // a body (`/patch <role> <text>`) parse successfully.
            // Single-arg commands just absorb the second token into
            // their argument string, which is still a real dispatch.
            let line = if cmd.takes_args {
                format!("/{} role body", cmd.name)
            } else {
                format!("/{}", cmd.name)
            };
            let parsed = parse_line(&line);
            assert!(
                !matches!(parsed, Command::Help),
                "/{} fell through to Command::Help — table and parse_line are out of sync",
                cmd.name
            );
        }
    }
}

/// One parsed user input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Address a specific role with `@<role> <text>`.
    SendTo {
        /// Role name from the `@<role>` prefix.
        role: String,
        /// Free-form prompt text.
        text: String,
    },
    /// Bare text — routed to the configured host role.
    SendToHost(String),
    /// `@all <text>` — broadcast to every running role.
    Broadcast(String),
    /// `/patch <role> <text>` — save a session-time correction for the
    /// named role. Persisted under `.coderoom/patches/<role>/`. Loaded
    /// on the role's next `/refresh` (or next `cr start`).
    Patch {
        /// Role whose priors will be patched.
        role: String,
        /// Correction text — written verbatim into the new patch file.
        text: String,
    },
    /// `/compact <role|all>` — ask one or all running roles to compact
    /// their live engine conversation context using a supervised
    /// engine-native primitive when supported.
    Compact(String),
    /// `/refresh <role>` — re-instantiate the role with the latest
    /// composed priors (shared.md + role.md + active patches). The
    /// old subprocess is dropped; a fresh one starts.
    Refresh(String),
    /// `/resume` lists saved CodeRoom room sessions. `/resume <id|index|latest>`
    /// switches the room to that saved set of per-role engine sessions.
    Resume(Option<String>),
    /// `/transcript <role>` — show the last few RoleSpoke entries for a
    /// role from `.coderoom/messages.jsonl`.
    Transcript(String),
    /// `/journal <role>` — ask the role to write a dated journal entry
    /// summarizing what it learned in this session. Persisted at
    /// `.coderoom/journal/YYYY-MM-DD/<role>.md`; auto-loaded into the
    /// role's priors on next spawn.
    Journal(String),
    /// `/welcome` — re-show the first-run welcome card on demand, even
    /// after the `.welcomed` marker has been written.
    Welcome,
    /// `/allow <tool>` — allow a tool in the session permission policy.
    Allow(String),
    /// `/deny <tool>` — deny a tool in the session permission policy.
    Deny(String),
    /// `/stop <role>` — terminate the named role's subprocess.
    Stop(String),
    /// `/halt` (no arg) interrupts the current in-flight turn for every
    /// running role. `/halt @role` interrupts that role only. Roles
    /// stay alive — only the turn ends. v0.2 § E.
    Halt(Option<String>),
    /// `/host <role>` — session-only host role swap.
    Host(String),
    /// `/help` — print the help banner.
    Help,
    /// `/exit` or empty input on EOF — leave the REPL.
    Exit,
    /// Empty input — re-prompt without doing anything.
    Empty,
}

/// Parse one line of user input. Pure function — no I/O.
#[must_use]
pub fn parse_line(input: &str) -> Command {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Command::Empty;
    }
    if let Some(rest) = trimmed.strip_prefix('/') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        return match cmd {
            "exit" | "quit" => Command::Exit,
            "stop" if !arg.is_empty() => {
                Command::Stop(arg.strip_prefix('@').unwrap_or(arg).to_owned())
            }
            "halt" => {
                let role = arg.strip_prefix('@').unwrap_or(arg).trim();
                if role.is_empty() {
                    Command::Halt(None)
                } else {
                    Command::Halt(Some(role.to_owned()))
                }
            }
            "host" if !arg.is_empty() => {
                Command::Host(arg.strip_prefix('@').unwrap_or(arg).to_owned())
            }
            "refresh" if !arg.is_empty() => {
                let role = arg.strip_prefix('@').unwrap_or(arg).to_owned();
                if role.is_empty() {
                    Command::Help
                } else {
                    Command::Refresh(role)
                }
            }
            "resume" => {
                let selector = arg.trim();
                if selector.is_empty() {
                    Command::Resume(None)
                } else {
                    Command::Resume(Some(selector.to_owned()))
                }
            }
            "transcript" if !arg.is_empty() => {
                let role = arg.strip_prefix('@').unwrap_or(arg).to_owned();
                if role.is_empty() {
                    Command::Help
                } else {
                    Command::Transcript(role)
                }
            }
            "journal" if !arg.is_empty() => {
                let role = arg.strip_prefix('@').unwrap_or(arg).to_owned();
                if role.is_empty() {
                    Command::Help
                } else {
                    Command::Journal(role)
                }
            }
            "patch" => parse_patch_arg(arg).unwrap_or(Command::Help),
            "compact" if !arg.is_empty() => {
                let target = arg.strip_prefix('@').unwrap_or(arg).trim();
                if target.is_empty() {
                    Command::Help
                } else {
                    Command::Compact(target.to_owned())
                }
            }
            "welcome" => Command::Welcome,
            "allow" if !arg.is_empty() => Command::Allow(arg.to_owned()),
            "deny" if !arg.is_empty() => Command::Deny(arg.to_owned()),
            // /help, /h, and any unknown slash command all fall through here.
            _ => Command::Help,
        };
    }
    if let Some(rest) = trimmed.strip_prefix('@') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let role = parts.next().unwrap_or("").to_owned();
        let text = parts.next().unwrap_or("").trim().to_owned();
        if role == "all" && !text.is_empty() {
            return Command::Broadcast(text);
        }
        if !role.is_empty() && !text.is_empty() {
            return Command::SendTo { role, text };
        }
    }
    Command::SendToHost(trimmed.to_owned())
}

/// Parse the argument string of `/patch <role> <text>`. Accepts both
/// `backend foo bar` and `@backend foo bar` for ergonomics. Returns
/// `None` (caller falls back to Help) if either side is empty.
fn parse_patch_arg(arg: &str) -> Option<Command> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let role_token = parts.next().unwrap_or("");
    let text = parts.next().unwrap_or("").trim();
    let role = role_token
        .strip_prefix('@')
        .unwrap_or(role_token)
        .to_owned();
    if role.is_empty() || text.is_empty() {
        return None;
    }
    Some(Command::Patch {
        role,
        text: text.to_owned(),
    })
}
