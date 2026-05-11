//! Engine-neutral helpers for role work reporting.

/// A parsed `cr-task` fenced block and the remaining assistant text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CrTaskExtraction {
    pub(crate) title: Option<String>,
    pub(crate) body: String,
}

/// Extract the first valid `cr-task` fenced block from `text`.
///
/// The block is display metadata only. The model never gets to choose
/// protocol identity or card ids through this path.
pub(crate) fn extract_cr_task(text: &str) -> CrTaskExtraction {
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    let Some(start_idx) = lines
        .iter()
        .position(|line| line.trim_end_matches('\n').trim() == "```cr-task")
    else {
        return CrTaskExtraction {
            title: None,
            body: text.to_owned(),
        };
    };
    let Some(end_rel) = lines[start_idx + 1..]
        .iter()
        .position(|line| line.trim_end_matches('\n').trim() == "```")
    else {
        return CrTaskExtraction {
            title: None,
            body: text.to_owned(),
        };
    };
    let end_idx = start_idx + 1 + end_rel;
    let title_source = lines[start_idx + 1..end_idx]
        .iter()
        .map(|line| line.trim())
        .find(|line| !line.is_empty());
    let title = title_source.map(sanitize_title);
    let mut body = String::new();
    for (idx, line) in lines.iter().enumerate() {
        if idx < start_idx || idx > end_idx {
            body.push_str(line);
        }
    }
    CrTaskExtraction {
        title,
        body: trim_blank_edges(&body),
    }
}

pub(crate) fn fallback_title(prompt: &str) -> String {
    // Auto-routed turns receive a brief shaped like
    //   "From @<role>: <real task description>"
    // (see `src/repl.rs::send_and_drain`). Without stripping that
    // prefix, the WorkCard title for the dispatched role's first
    // turn reads "From @host: …" — routing metadata leaks into a
    // user-facing surface. Strip the prefix before picking the
    // first non-blank line.
    let stripped = strip_route_brief_prefix(prompt);
    let first_line = stripped
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("role task");
    sanitize_title(first_line)
}

/// If `input` starts with the auto-route brief prefix
/// (`From @<role>: <body>`), return the slice after `": "`. Otherwise
/// return `input` unchanged. The role token must be a single word
/// (no whitespace) so we don't accidentally chew real content that
/// happens to start with "From @".
pub(crate) fn strip_route_brief_prefix(input: &str) -> &str {
    let trimmed = input.trim_start();
    let Some(rest) = trimmed.strip_prefix("From @") else {
        return input;
    };
    let Some(colon) = rest.find(':') else {
        return input;
    };
    let role_part = &rest[..colon];
    if role_part.is_empty() || role_part.contains(char::is_whitespace) {
        return input;
    }
    let after_colon = &rest[colon + 1..];
    after_colon.strip_prefix(' ').unwrap_or(after_colon)
}

/// If `text` starts with the role's own `@-mention` (the LLM
/// echoed its name back, which happens when the role token is in
/// the system prompt), return the text after that mention plus any
/// trailing whitespace or punctuation. Otherwise return `text`
/// unchanged. Without this strip, the renderer would print
/// `▎ @security @security <body>` — one badge from `first_prefix`,
/// one from the model's echo.
pub(crate) fn strip_leading_self_mention<'a>(text: &'a str, self_role: &str) -> &'a str {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix('@') else {
        return text;
    };
    let Some(after_role) = rest.strip_prefix(self_role) else {
        return text;
    };
    match after_role.chars().next() {
        // `@role` was the entire input — produce an empty tail.
        None => "",
        Some(c) if c.is_whitespace() || is_mention_terminator(c) => {
            after_role.trim_start_matches(|c: char| c.is_whitespace() || is_mention_terminator(c))
        }
        // Followed by a word char: this is a longer token like
        // `@securityteam`, not a self-mention. Leave it alone.
        Some(_) => text,
    }
}

fn is_mention_terminator(c: char) -> bool {
    // ASCII and common CJK trailing punctuation seen in role replies.
    matches!(
        c,
        ':' | ',' | '.' | '!' | '?' | ';' | '：' | '，' | '。' | '！' | '？' | '；'
    )
}

fn sanitize_title(input: &str) -> String {
    let mut title = input
        .split_whitespace()
        .take(20)
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        title.push_str("role task");
    }
    truncate_chars(&title, 160)
}

fn trim_blank_edges(input: &str) -> String {
    let mut lines: Vec<&str> = input.lines().collect();
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n")
    }
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_owned();
    }
    let mut out = input
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn cr_task_block_is_extracted_and_removed() {
        let input = "```cr-task\nScan repo permissions\n```\n\nHere is the result.";
        let extracted = extract_cr_task(input);
        assert_eq!(extracted.title.as_deref(), Some("Scan repo permissions"));
        assert_eq!(extracted.body, "Here is the result.");
    }

    #[test]
    fn malformed_cr_task_block_is_preserved() {
        let input = "```cr-task\nNo close\nbody";
        let extracted = extract_cr_task(input);
        assert_eq!(extracted.title, None);
        assert_eq!(extracted.body, input);
    }

    #[test]
    fn cr_task_title_is_capped_to_twenty_words() {
        let input = "```cr-task\none two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty twentyone\n```\nBody";
        let extracted = extract_cr_task(input);
        assert_eq!(
            extracted.title.as_deref(),
            Some(
                "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty"
            )
        );
    }

    #[test]
    fn fallback_title_uses_first_nonblank_prompt_line() {
        assert_eq!(fallback_title("\n\nscan repo\nextra"), "scan repo");
    }

    #[test]
    fn fallback_title_strips_auto_route_brief_prefix() {
        // The brief shape produced by `send_and_drain`:
        //   "From @host: <real task>"
        // Without the strip the WorkCard title for the dispatched
        // role's first turn would say "From @host: ..." which is
        // routing metadata, not the work.
        assert_eq!(
            fallback_title("From @host: scan the auth module"),
            "scan the auth module"
        );
        // Multi-line brief — strip prefix, then take first
        // non-blank line as today.
        assert_eq!(
            fallback_title("From @host: audit permission boundaries\nthen report back"),
            "audit permission boundaries"
        );
    }

    #[test]
    fn fallback_title_leaves_non_brief_prompts_unchanged() {
        // User content that happens to mention "From @x" later
        // shouldn't be mistaken for a brief.
        assert_eq!(
            fallback_title("scan repo — From @backend's notes"),
            "scan repo — From @backend's notes"
        );
        // No colon, no role token — keep as is.
        assert_eq!(
            fallback_title("From the latest report"),
            "From the latest report"
        );
    }

    #[test]
    fn strip_leading_self_mention_drops_echo_after_whitespace() {
        // The most common shape: model echoes its own name then a space.
        assert_eq!(
            strip_leading_self_mention("@security 方案如下：...", "security"),
            "方案如下：..."
        );
        // CJK punctuation right after the name (no space).
        assert_eq!(
            strip_leading_self_mention("@security：方案如下", "security"),
            "方案如下"
        );
        // ASCII colon variant.
        assert_eq!(
            strip_leading_self_mention("@security: here's my take", "security"),
            "here's my take"
        );
    }

    #[test]
    fn strip_leading_self_mention_preserves_other_mentions() {
        // @host at the start is NOT self for a security role —
        // that's a real address, keep it so auto-route sees the
        // mention.
        assert_eq!(
            strip_leading_self_mention("@host I recommend ...", "security"),
            "@host I recommend ..."
        );
    }

    #[test]
    fn strip_leading_self_mention_keeps_longer_tokens() {
        // `@securityteam` is a different identifier — don't strip.
        assert_eq!(
            strip_leading_self_mention("@securityteam meeting at 3", "security"),
            "@securityteam meeting at 3"
        );
    }

    #[test]
    fn strip_leading_self_mention_handles_mention_only_input() {
        assert_eq!(strip_leading_self_mention("@security", "security"), "");
    }
}
