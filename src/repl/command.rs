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
    /// `/refresh <role>` — re-instantiate the role with the latest
    /// composed priors (shared.md + role.md + active patches). The
    /// old subprocess is dropped; a fresh one starts.
    Refresh(String),
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
